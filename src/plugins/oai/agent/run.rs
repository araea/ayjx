//! 一轮 agent 对话：把历史展开成消息，循环请求，直到模型不再要工具。
//!
//! 循环的形状很朴素——请求、执行工具、把结果回填、再请求——值得写下来的只有
//! 三件事：
//!
//! 1. **模型的每一份回复都原样回填**。工具调用、思考块、签名都留在消息里，
//!    重放时上游才认得出自己上一轮说了什么；只留正文会让多轮工具调用散架。
//! 2. **轨迹只服务于页脚**。`tool_execution_start` 那套事件流没有了，改由这里
//!    在调用工具时记一笔，同名同参的连续调用合并成一条带次数的记录。
//! 3. **取消必须是干净的**。整轮可能被外层超时或「停止」指令 drop，所以工具执行
//!    直接挂在这个 future 上（见 [`super::bash`]），绝不 `tokio::spawn`。

use super::super::types::TraceStep;
use super::super::llm;
use super::AgentRun;
use rig_core::client::CompletionClient;
use rig_core::completion::message::{
    DocumentSourceKind, Image, Text, ToolResult, ToolResultContent, UserContent,
};
use rig_core::completion::{AssistantContent, CompletionModel, Message};

/// 页脚保留的工具调用条数上限；再多只记次数。
const TRACE_LIMIT: usize = 12;

/// 房间系统提示词里与运行环境有关的那一段。
///
/// 只写「这是哪儿、手边有什么」——人格与风格由房间提示词负责，工具细节由每个工具的
/// description 负责，这里再抄一遍就会开始过期。
const ROOM_BASE: &str = "\
你是运行在本机的中文助手，可以用工具读写文件、执行命令。
工作目录：{cwd}
回复要简洁，但关键依据不能省：引用了哪个文件、跑了哪条命令、拿到什么结果，都要说清楚。
工具报告的错误就是结果的一部分，照着它换个做法，别把它当成需要解释的现象。
工作目录里的路径直接写相对路径即可。";

/// 控制通道的用法说明；只有这一轮真的持有时才写进提示词。
const CONTROL_HINT: &str = "\
这一轮你还能直接操作机器人自己：用 bash 执行 `\"$AYJX_CTL_BIN\" --ctl \"<命令>\"`，
可以查看和修改插件开关与配置。具体用法见下面列的 skill，命令的回执会原样打回来。";

/// 跑一轮对话。
pub(crate) async fn run(run: AgentRun<'_>) -> anyhow::Result<super::AgentReply> {
    run_with_history(run, &[]).await
}

/// 跑一轮对话，`history` 是这轮之前已经发生过的往来。
///
/// 卡死且一个工具都还没动过时自动重来一次：上游抽风是常态，静静等满整轮预算太亏。
/// 动过工具就不能重放——同一份副作用做两遍比慢一点糟糕得多。
pub(crate) async fn run_with_history(
    run: AgentRun<'_>,
    history: &[super::super::types::ChatMessage],
) -> anyhow::Result<super::AgentReply> {
    match attempt(&run, history).await {
        Err(error) if error.stalled && run.retry_stalled && !error.used_tools => {
            warn!(target: "Plugin/OAI", "{}，重试一次", error.message);
            attempt(&run, history).await.map_err(|error| error.message)
        }
        Err(error) => Err(error.message),
        Ok(reply) => Ok(reply),
    }
}

/// 一次失败：区分「卡死」与「真的错了」，前者才值得重试。
struct Failure {
    message: anyhow::Error,
    stalled: bool,
    used_tools: bool,
}

async fn attempt(
    run: &AgentRun<'_>,
    history: &[super::super::types::ChatMessage],
) -> Result<super::AgentReply, Failure> {
    let context = Context::prepare(run, history).await?;
    let definitions = super::tools::definitions(run.tools, run.bridge.is_some());
    let client = llm::client(run.api_base, run.api_key).map_err(|message| Failure {
        message,
        stalled: false,
        used_tools: false,
    })?;
    let model = client.completion_model(run.model);

    let mut messages = context.messages;
    let mut trace = Trace::default();
    let mut used_tools = false;

    for _ in 0..run.max_steps.max(1) {
        let request = llm::request(messages.clone(), definitions.clone(), run.thinking);
        let response = match run.stall {
            Some(limit) => match tokio::time::timeout(limit, model.completion(request)).await {
                Ok(response) => response,
                Err(_) => {
                    return Err(Failure {
                        message: anyhow::anyhow!(
                            "模型请求静默超过 {} 秒无响应",
                            limit.as_secs()
                        ),
                        stalled: true,
                        used_tools,
                    });
                }
            },
            None => model.completion(request).await,
        }
        .map_err(|error| Failure {
            message: anyhow::anyhow!("{error}"),
            stalled: false,
            used_tools,
        })?;

        let calls: Vec<_> = response
            .choice
            .iter()
            .filter_map(|part| match part {
                AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        let text = llm::text_of(&response.choice);

        if calls.is_empty() {
            if text.trim().is_empty() {
                return Err(Failure {
                    message: anyhow::anyhow!("模型未返回最终回复"),
                    stalled: false,
                    used_tools,
                });
            }
            return Ok(super::AgentReply {
                text,
                // 实际应答的模型名由调用方补全（它才知道 `供应商/` 前缀）。
                model: Some(run.model.to_string()),
                trace: trace.steps,
                trace_overflow: trace.overflow,
            });
        }

        // 原样回填模型的这一份回复：工具调用、思考块与签名都要跟着走。
        messages.push(Message::Assistant {
            id: response.message_id.clone(),
            content: response.choice.clone(),
        });

        let mut results = Vec::with_capacity(calls.len());
        for call in &calls {
            let name = call.function.name.clone();
            trace.push(&name, super::tools::label(&call.function.arguments));
            if super::tools::is_side_effecting(&name) {
                used_tools = true;
            }
            let output = super::tools::execute(&name, &call.function.arguments, run, call.id.as_str())
                .await;
            results.push(UserContent::ToolResult(ToolResult {
                call: call.id.clone(),
                provider: call.provider.clone(),
                name,
                content: vec![ToolResultContent::text(output)],
            }));
        }
        messages.push(Message::User { content: results });
    }

    Err(Failure {
        message: anyhow::anyhow!("工具调用超过 {} 步仍未收尾", run.max_steps.max(1)),
        stalled: false,
        used_tools,
    })
}

/// 这一轮要发的开头消息。
struct Context {
    messages: Vec<Message>,
}

impl Context {
    async fn prepare(
        run: &AgentRun<'_>,
        history: &[super::super::types::ChatMessage],
    ) -> Result<Self, Failure> {
        let bad = |message: anyhow::Error| Failure {
            message,
            stalled: false,
            used_tools: false,
        };

        let index = skills(run).map_err(bad)?;
        let cwd = run.cwd.unwrap_or(run.dir);
        let mut system = match run.system_prompt {
            Some(explicit) => explicit.trim().to_string(),
            None => {
                let mut base = ROOM_BASE.replace("{cwd}", &cwd.display().to_string());
                if run.control {
                    base.push_str("\n\n");
                    base.push_str(CONTROL_HINT);
                }
                match run.append_system_prompt.trim() {
                    "" => base,
                    persona => format!("{base}\n\n---\n\n{persona}"),
                }
            }
        };
        if !index.is_empty() {
            system.push_str("\n\n---\n\n");
            system.push_str(&index);
        }

        let mut messages = Vec::new();
        if !system.trim().is_empty() {
            messages.push(Message::System { content: system });
        }
        for message in history {
            match message.role.as_str() {
                "user" => {
                    // 历史里的图片与这一条一样处理：先转成模型收得下的 data URL。
                    let content = user_content(&message.content, &message.images).await;
                    if !content.is_empty() {
                        messages.push(Message::User { content });
                    }
                }
                "assistant" => {
                    let clean = clean_history(&message.content);
                    if !clean.trim().is_empty() {
                        messages.push(Message::Assistant {
                            id: None,
                            content: vec![AssistantContent::Text(Text::new(clean))],
                        });
                    }
                }
                _ => {}
            }
        }

        let content = user_content(run.prompt, run.images).await;
        let content = if content.is_empty() {
            // 只发了图片、或者引用加空正文：给模型一句能落地的话，别让请求空着。
            vec![UserContent::Text(Text::new("请看图片。"))]
        } else {
            content
        };
        messages.push(Message::User { content });

        Ok(Self { messages })
    }
}

/// 正文 + 图片 → 一条用户消息的内容块。
async fn user_content(text: &str, images: &[String]) -> Vec<UserContent> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(UserContent::Text(Text::new(text)));
    }
    for url in images {
        let data_url = super::super::logic::to_data_url(url).await;
        content.push(UserContent::Image(Image {
            data: DocumentSourceKind::Url(data_url),
            media_type: None,
            detail: None,
            additional_params: None,
        }));
    }
    content
}

/// 历史里内嵌的 base64 图片重放只会撑爆上下文，留个占位即可。
fn clean_history(content: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap());
    re.replace_all(content, "[Image Created]").to_string()
}

/// 把 skill 铺进这一轮的目录，并返回给模型看的索引。
///
/// pi 时代的 `--skill` 是渐进披露：提示词里只有一行描述，正文要模型自己去读。
/// 这里照搬同一套：目录复制进 run dir，索引写清路径，模型用 `read` 打开。
fn skills(run: &AgentRun<'_>) -> anyhow::Result<String> {
    let mut lines = Vec::new();
    for source in run.skills {
        let (Some(name), Some(body)) = (
            source.file_name().and_then(|name| name.to_str()),
            read_skill(source),
        ) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let dir = run.dir.join("skills").join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), &body)?;
        let description = frontmatter(&body, "description").unwrap_or_else(|| name.to_string());
        lines.push(format!(
            "- {name}：{description}\n  正文：skills/{name}/SKILL.md（需要时用 read 读一遍）"
        ));
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "以下是这一轮随身的说明文档，用到哪份读哪份：\n{}",
        lines.join("\n")
    ))
}

/// skill 可以给目录（读其中的 SKILL.md），也可以直接给文件。
fn read_skill(source: &std::path::Path) -> Option<String> {
    let file = if source.is_dir() {
        source.join("SKILL.md")
    } else {
        source.to_path_buf()
    };
    std::fs::read_to_string(file).ok()
}

/// 取 SKILL.md 头部 frontmatter 里的一个字段。
fn frontmatter(body: &str, key: &str) -> Option<String> {
    let rest = body.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        if let Some(value) = line.trim().strip_prefix(&format!("{key}:")) {
            let value = value.trim().trim_matches(['"', '\'']).trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 页脚的工具轨迹：同名同参的连续调用合并，超出上限的只计数。
#[derive(Default)]
struct Trace {
    steps: Vec<TraceStep>,
    overflow: usize,
}
impl Trace {
    fn push(&mut self, name: &str, detail: String) {
        // 同一个工具连着用同样的参数（重试、分页）在页脚里排成一列毫无信息量，
        // 合并成一条带次数的记录。
        if let Some(last) = self.steps.last_mut()
            && last.name == name
            && last.detail == detail
        {
            last.repeats += 1;
        } else if self.steps.len() < TRACE_LIMIT {
            self.steps.push(TraceStep::new(name, detail));
        } else {
            self.overflow += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 一个按顺序回话的假模型端点：第 N 次请求回第 N 份剧本，用完停在那里。
    ///
    /// 返回（base, 收到的请求体, 服务任务）。请求体是解析过的 JSON，测试据此断言
    /// 「工具结果真的作为 tool 消息回填了」这类只有看线上请求才看得出来的事。
    pub(crate) async fn scripted_model(
        replies: Vec<serde_json::Value>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let task_seen = seen.clone();

        let task = tokio::spawn(async move {
            let mut index = 0;
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    break;
                }
                let mut length = 0;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
                let mut payload = vec![0; length];
                let _ = reader.read_exact(&mut payload).await;
                let _ = index;
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    task_seen.lock().unwrap().push(value);
                }
                let reply = &replies[index.min(replies.len() - 1)];
                index += 1;
                let finish = if reply["tool_calls"].is_array() {
                    "tool_calls"
                } else {
                    "stop"
                };
                let body = serde_json::json!({
                    "id": "x",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "fake",
                    "choices": [{
                        "index": 0,
                        "message": reply,
                        "finish_reason": finish,
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                })
                .to_string();
                let _ = reader
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (base, seen, task)
    }

    /// 一轮真的会调工具：模型要 bash，结果回填，模型再据此收尾。
    #[tokio::test]
    async fn a_tool_call_round_trips_and_lands_in_the_footer_trace() {
        let dir = std::env::temp_dir().join(format!("ayjx-run-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, seen, server) = scripted_model(vec![
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"printf AYJX_OK\"}"}
                }]
            }),
            serde_json::json!({"role": "assistant", "content": "命令输出是 AYJX_OK"}),
        ])
        .await;

        let reply = run(AgentRun {
            api_base: &base,
            api_key: "test-only",
            dir: &dir,
            cwd: Some(&dir),
            model: "fake-model",
            prompt: "跑一条命令看看",
            ..AgentRun::new()
        })
        .await
        .unwrap();
        assert_eq!(reply.text, "命令输出是 AYJX_OK");
        assert_eq!(reply.trace.len(), 1);
        assert_eq!(reply.trace[0].name, "bash");
        assert_eq!(reply.trace[0].detail, "printf AYJX_OK");

        // 第二次请求必须带上工具结果，且它是一条 tool 消息。
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2, "工具循环应该正好两轮");
        let messages = requests[1]["messages"].as_array().unwrap();
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("工具结果要作为 tool 消息回填");
        assert!(
            tool_message["content"].as_str().unwrap().contains("AYJX_OK"),
            "{tool_message}"
        );
        // 带上工具的请求里必须有 tools 定义。
        assert!(requests[0]["tools"].as_array().is_some_and(|t| !t.is_empty()));
        // 思考强度走 additional_params，扁平在顶层。
        assert!(requests[0].get("reasoning_effort").is_none());

        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 普通房间那一侧没有工具；这里确认 agent 的请求确实带着工具与思考强度。
    #[tokio::test]
    async fn the_request_carries_tools_and_reasoning_effort() {
        let dir = std::env::temp_dir().join(format!("ayjx-run-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, seen, server) =
            scripted_model(vec![serde_json::json!({"role": "assistant", "content": "好"})]).await;

        run(AgentRun {
            api_base: &base,
            api_key: "test-only",
            dir: &dir,
            model: "fake-model",
            thinking: Some("high"),
            prompt: "在吗",
            ..AgentRun::new()
        })
        .await
        .unwrap();

        let requests = seen.lock().unwrap();
        assert_eq!(requests[0]["reasoning_effort"], "high");
        let names: Vec<&str> = requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"bash"), "{names:?}");
        assert!(!names.contains(&"satori_action"), "没有聊天界面就不该有它：{names:?}");
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 一个把每轮都用来调工具的模型必须被步数上限拦住，而不是转下去。
    #[tokio::test]
    async fn endless_tool_calls_stop_at_the_step_limit() {
        let dir = std::env::temp_dir().join(format!("ayjx-run-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, _, server) = scripted_model(vec![serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"nope\"}"}
            }]
        })])
        .await;

        let error = run(AgentRun {
            api_base: &base,
            api_key: "test-only",
            dir: &dir,
            model: "fake-model",
            max_steps: 3,
            prompt: "随便",
            ..AgentRun::new()
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("3 步"), "{error}");
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 模型不给正文也不调工具时，错误要说清是「没返回」而不是「超时」。
    #[tokio::test]
    async fn an_empty_answer_is_reported_as_such() {
        let dir = std::env::temp_dir().join(format!("ayjx-run-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, _, server) =
            scripted_model(vec![serde_json::json!({"role": "assistant", "content": "   "})]).await;
        let error = run(AgentRun {
            api_base: &base,
            api_key: "test-only",
            dir: &dir,
            model: "fake-model",
            prompt: "在吗",
            ..AgentRun::new()
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("未返回最终回复"), "{error}");
        server.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 真实模型的接入测试：确认上游的工具调用格式与本实现真的对得上
    /// （假服务器只能证明我们自己拼得对，证明不了对面认不认）。
    ///
    /// 需要 `AYJX_AGENT_LIVE_BASE` / `_KEY` / `_MODEL`。
    #[tokio::test]
    #[ignore = "调用真实模型接口，需要网络"]
    async fn live_agent_uses_a_tool_and_answers_from_its_output() {
        let base = std::env::var("AYJX_AGENT_LIVE_BASE").expect("请设置 AYJX_AGENT_LIVE_BASE");
        let key = std::env::var("AYJX_AGENT_LIVE_KEY").expect("请设置 AYJX_AGENT_LIVE_KEY");
        let model = std::env::var("AYJX_AGENT_LIVE_MODEL").expect("请设置 AYJX_AGENT_LIVE_MODEL");
        let dir = std::env::temp_dir().join(format!("ayjx-live-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();

        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            run(AgentRun {
                api_base: &base,
                api_key: &key,
                dir: &dir,
                cwd: Some(&dir),
                model: &model,
                prompt: "请执行一次 bash 命令 printf AYJX_AGENT_TOOL_OK，然后只回答命令的输出。",
                ..AgentRun::new()
            }),
        )
        .await
        .unwrap()
        .unwrap();

        println!("模型：{model}；回复：{}；工具：{:?}", reply.text, reply.trace);
        assert!(reply.text.contains("AYJX_AGENT_TOOL_OK"), "{}", reply.text);
        assert!(
            reply.trace.iter().any(|step| step.name == "bash"),
            "应当真的调过 bash：{:?}",
            reply.trace
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn repeated_tool_calls_merge_and_overflow_is_counted() {
        let mut trace = Trace::default();
        trace.push("satori_context", "".into());
        trace.push("satori_context", "".into());
        trace.push("bash", "uname -a".into());
        assert_eq!(trace.steps.len(), 2);
        assert_eq!(trace.steps[0].repeats, 2);
        assert_eq!(trace.steps[1].detail, "uname -a");

        for index in 0..TRACE_LIMIT + 3 {
            trace.push("bash", format!("cmd-{index}"));
        }
        assert_eq!(trace.steps.len(), TRACE_LIMIT);
        assert_eq!(trace.overflow, 5);
    }

    #[test]
    fn skill_frontmatter_supplies_the_index_line() {
        let body = "---\nname: demo\ndescription: 一句话说清什么时候读它\n---\n\n正文\n";
        assert_eq!(
            frontmatter(body, "description").as_deref(),
            Some("一句话说清什么时候读它")
        );
        assert_eq!(frontmatter("没有头部\n", "description"), None);
    }

    #[tokio::test]
    async fn skills_are_copied_into_the_run_directory_and_indexed() {
        let source = std::env::temp_dir().join(format!("ayjx-skill-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\ndescription: 聊天界面怎么用\n---\n\n正文\n",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("ayjx-run-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();

        let provided = vec![source.clone()];
        let run = AgentRun {
            dir: &dir,
            skills: &provided,
            ..AgentRun::new()
        };
        let index = skills(&run).unwrap();
        let name = source.file_name().unwrap().to_str().unwrap();
        assert!(index.contains("聊天界面怎么用"), "{index}");
        assert!(index.contains(&format!("skills/{name}/SKILL.md")), "{index}");
        assert!(dir.join("skills").join(name).join("SKILL.md").exists());

        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(source);
    }

    #[test]
    fn image_placeholders_replace_history_image_payloads() {
        let long = format!("![x](data:image/png;base64,{})", "A".repeat(400));
        assert_eq!(clean_history(&long), "[Image Created]");
        assert_eq!(clean_history("普通正文"), "普通正文");
    }

    #[tokio::test]
    async fn an_empty_prompt_with_images_still_has_something_to_send() {
        let dir: &Path = Path::new(".");
        let run = AgentRun {
            dir,
            model: "example-model",
            prompt: "",
            ..AgentRun::new()
        };
        let context = Context::prepare(&run, &[]).await.ok().unwrap();
        // 只有 system + 一条「请看图片。」的用户消息。
        assert_eq!(context.messages.len(), 2);
    }
}
