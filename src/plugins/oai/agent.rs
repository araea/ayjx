//! `pi` 房间的 Responses 智能体运行时。
//!
//! 为什么不用 Chat Completions：中转站把 `/v1/chat/completions` 实现成
//! `/v1/responses` 的兼容垫片，途中会丢掉托管工具、推理档位与推理态的跨轮复用。
//! 直接对话 Responses 端点能拿回三件事——
//!
//! 1. **托管 `web_search`**：由服务端执行检索并回填带 `url_citation` 的正文。
//!    本机抓取公共搜索引擎在移动网络下几乎必被反爬拦截，这是「网页搜索不好用」
//!    的根因；托管检索把它变成一次稳定的服务端调用。
//! 2. **`reasoning.effort`**：闲聊场景压到低档，等待时间显著下降。
//! 3. **`reasoning.encrypted_content`**：`store=false` 下把推理态带进下一轮，
//!    工具循环不必每轮重新想一遍。
//!
//! 客户端是手写的薄封装而非 SDK 类型：中转站的字段与官方规范常有出入，用
//! `serde_json::Value` 承载输出项可以原样回灌未知条目，不会因为多一个字段就
//! 整轮反序列化失败。
//!
//! 端点能力靠一次探测确定并缓存：不支持 `/responses` 的中转会返回
//! [`AgentError::Unsupported`]，调用方据此回落到原有的 Chat Completions 链路。

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::harness::{HarnessConfig, MAX_TOOL_ROUNDS, ToolRun};

/// 单次 HTTP 请求的上限；整轮预算由调用方的总超时把关。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// 正文引用到的网页来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub title: String,
    pub url: String,
}

/// 一次回复的完整产出。
#[derive(Debug, Default)]
pub(crate) struct AgentOutcome {
    pub text: String,
    pub sources: Vec<Source>,
    /// 人类可读的工具调用轨迹，用于回复卡片的页脚。
    pub trace: Vec<String>,
}

/// 进度事件；调用方可据此在长耗时请求中给用户一点反馈。
#[derive(Debug, Clone)]
pub(crate) enum Progress {
    /// 托管检索发起的查询。
    HostedSearch(String),
    /// 本机工具执行完成（携带摘要）。
    Tool(String),
}

pub(crate) enum AgentError {
    /// 端点不支持 Responses，调用方应回落到 Chat Completions。
    Unsupported(String),
    /// 服务端以 4xx 拒绝了请求；可能只是某个可选参数不被支持。
    Rejected { status: u16, detail: String },
    Failed(anyhow::Error),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Unsupported(reason) => write!(f, "{reason}"),
            AgentError::Rejected { status, detail } => write!(f, "HTTP {status}：{detail}"),
            AgentError::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

pub(crate) struct AgentRequest {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    /// 系统提示（人设 + 本机环境 + 工具策略）。
    pub instructions: String,
    /// Responses 输入项，已含历史消息与本轮提问。
    pub input: Vec<Value>,
    pub harness: HarnessConfig,
    pub reasoning_effort: Option<String>,
    /// 提示缓存键：同一房间复用前缀缓存，明显降低重复对话的首字延迟。
    pub cache_key: Option<String>,
    pub progress: Option<tokio::sync::mpsc::UnboundedSender<Progress>>,
}

/// 端点对可选请求参数的支持情况。
///
/// 中转站对 Responses 的实现深浅不一：有的没有托管检索，有的不认 `include`，
/// 有的把 `reasoning` 直接当非法字段拒掉。与其为每家写死配置，不如第一次请求
/// 时按报错逐项关掉再重试，并把结论按「端点 + 模型」缓存起来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Capabilities {
    hosted_search: bool,
    encrypted_reasoning: bool,
    reasoning_effort: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            hosted_search: true,
            encrypted_reasoning: true,
            reasoning_effort: true,
        }
    }
}

impl Capabilities {
    /// 关掉一项可选参数并说明关的是什么；`None` 表示已退无可退。
    ///
    /// 中转站的报错常常只有一句「bad response status code 400」，指望它点名字段是
    /// 不现实的。所以按「丢了最不可惜」的顺序逐项后退，托管检索留到最后——它才是
    /// 联网质量的关键。
    fn downgrade(&mut self, detail: &str) -> Option<&'static str> {
        let lower = detail.to_ascii_lowercase();
        // 报错点名了字段就直接命中，省掉中间几次无谓重试。
        if self.hosted_search && lower.contains("web_search") {
            self.hosted_search = false;
            return Some("托管 web_search");
        }
        if self.encrypted_reasoning
            && (lower.contains("include") || lower.contains("encrypted_content"))
        {
            self.encrypted_reasoning = false;
            return Some("reasoning.encrypted_content");
        }
        if self.reasoning_effort && (lower.contains("reasoning") || lower.contains("effort")) {
            self.reasoning_effort = false;
            return Some("reasoning.effort");
        }
        if self.encrypted_reasoning {
            self.encrypted_reasoning = false;
            return Some("reasoning.encrypted_content");
        }
        if self.reasoning_effort {
            self.reasoning_effort = false;
            return Some("reasoning.effort");
        }
        if self.hosted_search {
            self.hosted_search = false;
            return Some("托管 web_search");
        }
        None
    }
}

/// 这些 4xx 是真正的业务错误，改请求形态救不回来，重试只会白烧配额。
fn is_terminal_rejection(status: u16, detail: &str) -> bool {
    if matches!(status, 401 | 402 | 403 | 429) {
        return true;
    }
    let lower = detail.to_ascii_lowercase();
    [
        "rate limit",
        "quota",
        "insufficient",
        "billing",
        "api key",
        "unauthorized",
        "permission",
        "context length",
        "too many tokens",
        "maximum context",
        "content policy",
        "model_not_found",
        "does not exist",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn capability_cache() -> &'static Mutex<HashMap<String, Capabilities>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Capabilities>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_capabilities(key: &str) -> Option<Capabilities> {
    capability_cache().lock().ok()?.get(key).copied()
}

fn remember_capabilities(key: &str, caps: Capabilities) {
    if let Ok(mut cache) = capability_cache().lock() {
        cache.insert(key.to_string(), caps);
    }
}

/// 把本机历史消息转成 Responses 输入项。
///
/// 图片按多模态 `input_image` 内联；助手消息用简化的字符串形态，避免带上
/// 只在服务端有效的 `id`/`status`。
pub(crate) fn user_item(text: &str, images: &[String]) -> Option<Value> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(json!({"type": "input_text", "text": text}));
    }
    for image in images {
        content.push(json!({"type": "input_image", "image_url": image, "detail": "auto"}));
    }
    if content.is_empty() {
        return None;
    }
    Some(json!({"type": "message", "role": "user", "content": content}))
}

pub(crate) fn assistant_item(text: &str) -> Option<Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(json!({"role": "assistant", "content": text}))
}

/// 执行「模型 → 工具 → 回灌」循环，直到模型给出最终文本。
pub(crate) async fn run(request: AgentRequest) -> Result<AgentOutcome, AgentError> {
    let endpoint = format!("{}/responses", request.api_base.trim_end_matches('/'));
    let cache_key = format!("{}|{}", request.api_base, request.model);
    let mut caps = cached_capabilities(&cache_key).unwrap_or_default();
    caps.hosted_search &= request.harness.hosted_web_search;

    let mut input = request.input.clone();
    let mut trace: Vec<String> = Vec::new();
    let mut sources: Vec<Source> = Vec::new();

    for _ in 0..MAX_TOOL_ROUNDS {
        let harness = HarnessConfig {
            hosted_web_search: caps.hosted_search,
            ..request.harness
        };

        // 内层循环只处理「换个请求形态再试」，不消耗工具轮次预算。
        let response = loop {
            let body = build_body(&request, &input, &harness, caps);
            match post(&endpoint, &request.api_key, &body).await {
                Ok(response) => break response,
                Err(AgentError::Rejected { status, detail }) => {
                    if is_terminal_rejection(status, &detail) {
                        return Err(AgentError::Rejected { status, detail });
                    }
                    match caps.downgrade(&detail) {
                        Some(dropped) => {
                            warn!(target: "Plugin/OAI", "端点拒绝了请求（HTTP {status}：{detail}），去掉 {dropped} 后重试");
                            remember_capabilities(&cache_key, caps);
                            continue;
                        }
                        None => return Err(AgentError::Rejected { status, detail }),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        remember_capabilities(&cache_key, caps);

        let output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if output.is_empty() {
            return Err(AgentError::Failed(anyhow::anyhow!(
                "接口未返回任何输出（status: {}）",
                response
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }

        collect_sources(&output, &mut sources);
        for query in hosted_search_queries(&output) {
            trace.push(format!("web_search {query}"));
            if let Some(sender) = &request.progress {
                let _ = sender.send(Progress::HostedSearch(query));
            }
        }

        let calls = function_calls(&output);
        if calls.is_empty() {
            let text = final_text(&output);
            if text.trim().is_empty() {
                return Err(AgentError::Failed(anyhow::anyhow!("接口返回了空回复")));
            }
            return Ok(AgentOutcome {
                text,
                sources,
                trace,
            });
        }

        // 推理态与函数调用原样回灌：`store=false` 下这是模型记住上一轮思路的唯一途径。
        input.extend(output.iter().filter(|item| is_replayable_item(item)).cloned());

        let executions = calls.iter().map(async |call| {
            let run: ToolRun =
                super::harness::execute_tool(&call.name, &call.arguments, harness).await;
            (call.call_id.clone(), run)
        });
        for (call_id, run) in futures_util::future::join_all(executions).await {
            trace.push(run.summary.clone());
            if let Some(sender) = &request.progress {
                let _ = sender.send(Progress::Tool(run.summary));
            }
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": run.output,
            }));
        }
    }

    Err(AgentError::Failed(anyhow::anyhow!(
        "工具调用超过 {MAX_TOOL_ROUNDS} 轮，已停止以避免无限循环"
    )))
}

fn build_body(
    request: &AgentRequest,
    input: &[Value],
    harness: &HarnessConfig,
    caps: Capabilities,
) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    if caps.hosted_search {
        tools.push(json!({"type": "web_search"}));
    }
    for spec in super::harness::tool_specs(*harness) {
        tools.push(json!({
            "type": "function",
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
            "strict": false,
        }));
    }

    let mut body = json!({
        "model": request.model,
        "input": input,
        "instructions": request.instructions,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        // 房间历史保存在本地，不需要服务端会话状态；换来的是可以自由编辑历史。
        "store": false,
    });
    if caps.encrypted_reasoning {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if caps.reasoning_effort
        && let Some(effort) = &request.reasoning_effort
    {
        body["reasoning"] = json!({"effort": effort});
    }
    if let Some(cache_key) = &request.cache_key {
        body["prompt_cache_key"] = json!(cache_key);
    }
    body
}

async fn post(endpoint: &str, api_key: &str, body: &Value) -> Result<Value, AgentError> {
    let response = crate::http::client()
        .post(endpoint)
        .bearer_auth(api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .json(body)
        .send()
        .await
        .map_err(|error| AgentError::Failed(error.into()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AgentError::Failed(error.into()))?;

    if !status.is_success() {
        let detail = error_detail(&text);
        // 老中转只实现了 Chat Completions；这类失败必须与真正的调用错误区分开，
        // 否则会把可回落的场景变成一次面向用户的报错。
        if matches!(status.as_u16(), 404 | 405 | 501) || mentions_missing_endpoint(&detail) {
            return Err(AgentError::Unsupported(format!(
                "端点不支持 Responses（HTTP {status}）：{detail}"
            )));
        }
        return Err(AgentError::Rejected {
            status: status.as_u16(),
            detail,
        });
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|error| AgentError::Failed(anyhow::anyhow!("响应不是合法 JSON：{error}")))?;
    if let Some(error) = value.get("error").filter(|value| !value.is_null()) {
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
            .to_string();
        if mentions_missing_endpoint(&detail) {
            return Err(AgentError::Unsupported(detail));
        }
        return Err(AgentError::Failed(anyhow::anyhow!(detail)));
    }
    Ok(value)
}

fn error_detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| super::search::truncate_chars(body.trim(), 300))
}

fn mentions_missing_endpoint(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    ["unrecognized request url", "not found", "no such endpoint", "unknown path"]
        .iter()
        .any(|needle| lower.contains(needle))
}

struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// 可以安全回灌给下一轮的输出项。
///
/// 排除服务端自产的托管调用结果（重放它们没有意义，部分中转还会因为缺少配对的
/// 输出项而报错），保留推理态与函数调用。
fn is_replayable_item(item: &Value) -> bool {
    matches!(item_type(item), "reasoning" | "function_call" | "message")
}

fn function_calls(output: &[Value]) -> Vec<FunctionCall> {
    output
        .iter()
        .filter(|item| item_type(item) == "function_call")
        .filter_map(|item| {
            Some(FunctionCall {
                call_id: item.get("call_id").and_then(Value::as_str)?.to_string(),
                name: item.get("name").and_then(Value::as_str)?.to_string(),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            })
        })
        .collect()
}

fn hosted_search_queries(output: &[Value]) -> Vec<String> {
    output
        .iter()
        .filter(|item| item_type(item) == "web_search_call")
        .filter_map(|item| {
            let action = item.get("action")?;
            let query = action
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    action
                        .get("queries")
                        .and_then(Value::as_array)
                        .and_then(|queries| queries.first())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .or_else(|| {
                    action
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })?;
            Some(super::search::truncate_chars(&query, 60))
        })
        .collect()
}

fn final_text(output: &[Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in output.iter().filter(|item| item_type(item) == "message") {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for entry in content {
            match entry.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    if let Some(text) = entry.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        parts.push(text.to_string());
                    }
                }
                Some("refusal") => {
                    if let Some(text) = entry.get("refusal").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    parts.join("\n\n").trim().to_string()
}

fn collect_sources(output: &[Value], sources: &mut Vec<Source>) {
    for item in output.iter().filter(|item| item_type(item) == "message") {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for entry in content {
            let Some(annotations) = entry.get("annotations").and_then(Value::as_array) else {
                continue;
            };
            for annotation in annotations {
                if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
                    continue;
                }
                let Some(url) = annotation.get("url").and_then(Value::as_str) else {
                    continue;
                };
                if sources.iter().any(|source| source.url == url) {
                    continue;
                }
                let title = annotation
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or(url)
                    .to_string();
                sources.push(Source {
                    title,
                    url: url.to_string(),
                });
            }
        }
    }
}

/// 组装本机环境说明与工具使用策略，拼在人设提示之后。
///
/// 明确写清「先搜后读」「引用来源」「面向图片卡片作答」，比让模型自行摸索
/// 更能稳定输出质量——这也是 pi-agent 系 harness 的一贯做法。
pub(crate) fn build_instructions(persona: &str, hosted_search: bool, room: &str) -> String {
    let now = chrono::Local::now();
    let mut instructions = String::new();
    if !persona.trim().is_empty() {
        instructions.push_str(persona.trim());
        instructions.push_str("\n\n");
    }
    instructions.push_str(&format!(
        "# 运行环境\n\
         - 你是 QQ 群聊里的助手「{room}」，回复会被排版成一张图片卡片发出，接收者无法点击链接。\n\
         - 当前时间：{}（{}）。凡是「今天/最新/现在」一类的问题，一律以此为准并联网核实。\n\
         - 本机：{} {}，工作目录 {}。\n\n",
        now.format("%Y-%m-%d %H:%M"),
        now.format("%A"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "未知".to_string()),
    ));
    instructions.push_str(
        "# 工具策略\n\
         - 事实性、时效性、可外部验证的问题：必须先检索，再用 web_fetch 打开最相关的 1-3 个来源核对细节，禁止仅凭摘要或记忆作答。\n\
         - 需要查看本机文件、进程或运行命令时用 shell；命令必须非交互，一次给全参数。\n\
         - 相互独立的工具调用请在同一轮一起发出，它们会并发执行，能明显缩短等待。\n\
         - 工具失败时换个查询或换个来源重试一次，仍失败就如实说明，不要编造结果。\n\n",
    );
    if hosted_search {
        instructions.push_str(
            "- 检索用内置的 web_search 工具；它会自动附带来源标注。\n\n",
        );
    }
    instructions.push_str(
        "# 回答要求\n\
         - 默认简体中文，直入正题，不要寒暄和自我复述。\n\
         - 用 Markdown 组织：小标题、要点列表、必要时用表格；代码和命令放进代码块并标注语言。\n\
         - 结论先行，再给依据；长度与问题复杂度匹配，简单问题就一两句话。\n\
         - 引用外部信息时用 [标题](链接) 标注来源，让读者知道结论出处。\n",
    );
    instructions
}

/// 组装一条给用户看的耗时说明。
pub(crate) fn format_elapsed(started: Instant) -> String {
    let seconds = started.elapsed().as_secs_f32();
    if seconds >= 60.0 {
        format!("{}分{:.0}秒", (seconds / 60.0) as u32, seconds % 60.0)
    } else {
        format!("{seconds:.1}秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness() -> HarnessConfig {
        HarnessConfig {
            shell_timeout_seconds: 30,
            shell_max_output_bytes: 4096,
            web_search_results: 5,
            web_fetch_max_chars: 4000,
            web_fetch_timeout_seconds: 20,
            hosted_web_search: true,
        }
    }

    fn request() -> AgentRequest {
        AgentRequest {
            api_base: "https://example.com/v1".into(),
            api_key: "sk-test".into(),
            model: "gpt-5.6-luna".into(),
            instructions: "persona".into(),
            input: vec![],
            harness: harness(),
            reasoning_effort: Some("low".into()),
            cache_key: Some("ayjx:pi".into()),
            progress: None,
        }
    }

    #[test]
    fn hosted_search_replaces_local_search_in_the_request() {
        let hosted = build_body(&request(), &[], &harness(), Capabilities::default());
        let names: Vec<&str> = hosted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool.get("name")
                    .or_else(|| tool.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(names[0], "web_search");
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"web_fetch"));
        // 托管检索在场时不再挂本机同名工具。
        assert_eq!(names.iter().filter(|name| **name == "web_search").count(), 1);

        let local = HarnessConfig {
            hosted_web_search: false,
            ..harness()
        };
        let degraded = build_body(
            &request(),
            &[],
            &local,
            Capabilities {
                hosted_search: false,
                ..Capabilities::default()
            },
        );
        let names: Vec<&str> = degraded["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or(""))
            .collect();
        assert!(names.contains(&"web_search"));
        assert!(degraded["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["type"] == "function"));
    }

    #[test]
    fn request_stays_stateless_and_cacheable() {
        let body = build_body(&request(), &[], &harness(), Capabilities::default());
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["reasoning"]["effort"], json!("low"));
        assert_eq!(body["prompt_cache_key"], json!("ayjx:pi"));
    }

    #[test]
    fn extracts_text_calls_and_citations() {
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "abc"}),
            json!({"type": "web_search_call", "id": "ws_1", "status": "completed",
                   "action": {"type": "search", "query": "ayjx harness"}}),
            json!({"type": "function_call", "id": "fc_1", "call_id": "call_1",
                   "name": "shell", "arguments": "{\"command\":\"ls\"}"}),
            json!({"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "结论。",
                 "annotations": [{"type": "url_citation", "title": "来源", "url": "https://example.com/a"},
                                 {"type": "url_citation", "title": "重复", "url": "https://example.com/a"}]}
            ]}),
        ];

        assert_eq!(final_text(&output), "结论。");
        assert_eq!(hosted_search_queries(&output), vec!["ayjx harness"]);

        let calls = function_calls(&output);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].call_id, "call_1");

        let mut sources = Vec::new();
        collect_sources(&output, &mut sources);
        assert_eq!(
            sources,
            vec![Source {
                title: "来源".into(),
                url: "https://example.com/a".into()
            }]
        );

        // 托管调用结果不回灌，推理态与函数调用要原样带走。
        let replayed: Vec<&str> = output
            .iter()
            .filter(|item| is_replayable_item(item))
            .map(item_type)
            .collect();
        assert_eq!(replayed, vec!["reasoning", "function_call", "message"]);
    }

    #[test]
    fn classifies_endpoint_and_tool_failures() {
        assert!(mentions_missing_endpoint(
            "Unrecognized request URL (POST /v1/responses)"
        ));
        assert!(!mentions_missing_endpoint("rate limit exceeded"));
    }

    #[test]
    fn unsupported_parameters_are_dropped_one_at_a_time() {
        let mut caps = Capabilities::default();
        assert_eq!(
            caps.downgrade("tool 'web_search' is not supported"),
            Some("托管 web_search")
        );
        assert!(!caps.hosted_search);
        assert!(caps.encrypted_reasoning);

        // 中转常见的不点名 400：按「丢了最不可惜」的顺序后退，托管检索留到最后。
        let mut caps = Capabilities::default();
        assert_eq!(
            caps.downgrade("bad response status code 400"),
            Some("reasoning.encrypted_content")
        );
        assert_eq!(
            caps.downgrade("bad response status code 400"),
            Some("reasoning.effort")
        );
        assert_eq!(
            caps.downgrade("bad response status code 400"),
            Some("托管 web_search")
        );
        assert_eq!(caps.downgrade("bad response status code 400"), None);
    }

    #[test]
    fn business_errors_are_never_retried_as_parameter_problems() {
        assert!(is_terminal_rejection(429, "slow down"));
        assert!(is_terminal_rejection(400, "maximum context length exceeded"));
        assert!(is_terminal_rejection(404, "The model does not exist"));
        assert!(!is_terminal_rejection(400, "bad response status code 400"));
    }

    #[test]
    fn degraded_requests_drop_the_optional_fields() {
        let minimal = Capabilities {
            hosted_search: false,
            encrypted_reasoning: false,
            reasoning_effort: false,
        };
        let body = build_body(&request(), &[], &harness(), minimal);
        assert!(body.get("include").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("max_tool_calls").is_none());
        assert_eq!(body["store"], json!(false));
        assert!(body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["type"] == "function"));
    }

    #[test]
    fn instructions_carry_date_and_tool_policy() {
        let instructions = build_instructions("你是猫娘。", true, "pi");
        assert!(instructions.starts_with("你是猫娘。"));
        assert!(instructions.contains(&chrono::Local::now().format("%Y-%m-%d").to_string()));
        assert!(instructions.contains("web_fetch"));
        assert!(instructions.contains("并发执行"));
    }

    #[test]
    fn builds_multimodal_input_items() {
        let item = user_item("你好", &["data:image/png;base64,AAA".to_string()]).unwrap();
        assert_eq!(item["role"], json!("user"));
        assert_eq!(item["content"][0]["type"], json!("input_text"));
        assert_eq!(item["content"][1]["type"], json!("input_image"));
        assert!(user_item("   ", &[]).is_none());
        assert!(assistant_item("").is_none());
    }
}
