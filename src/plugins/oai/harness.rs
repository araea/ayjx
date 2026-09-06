//! `pi` 房间使用的本机工具执行层。
//!
//! 工具集沿用 pi-agent / oh-my-pi 的划分：`shell` 负责本机，`web_search` 找线索，
//! `web_fetch` 取证据。三者都由模型按需并发调用，结果以 call id 回灌，直到模型
//! 给出最终自然语言。
//!
//! 工具规格用中立的 JSON 描述，Chat Completions 与 Responses 两条链路各自转换：
//! 同一份定义不必为了迁移接口重写一遍。

use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// 单次回复内允许的工具轮次上限，防止模型陷入死循环。
pub(crate) const MAX_TOOL_ROUNDS: usize = 12;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HarnessConfig {
    pub shell_timeout_seconds: u64,
    pub shell_max_output_bytes: usize,
    pub web_search_results: usize,
    pub web_fetch_max_chars: usize,
    pub web_fetch_timeout_seconds: u64,
    /// 服务端已提供托管 `web_search` 工具时置真，本地搜索工具随之下线，
    /// 避免同名工具打架，也省掉一次注定被反爬拦截的抓取。
    pub hosted_web_search: bool,
}

/// 与具体 API 形态无关的工具定义。
pub(crate) struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// 一次工具执行的结果：`output` 回灌给模型，`summary` 进人类可读的调用轨迹。
pub(crate) struct ToolRun {
    pub output: String,
    pub summary: String,
    pub failed: bool,
}

pub(crate) fn tool_specs(config: HarnessConfig) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec {
            name: "shell",
            description: "在本机执行 shell 命令，拥有完整权限，无需用户确认。用于查看文件、运行程序、检查系统状态。命令是非交互的，不要执行需要人工输入或长期驻留的进程。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "完整的 shell 命令"},
                    "cwd": {"type": "string", "description": "可选的工作目录"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "web_fetch",
            description: "抓取一个网页并返回其正文纯文本。搜索只给摘要，回答需要具体数据、原文措辞或结论细节时，必须用本工具打开来源页确认，不要凭摘要推测。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "要抓取的 http/https 地址"},
                    "max_chars": {"type": "integer", "minimum": 500, "maximum": 40000, "description": "返回正文的字符上限"}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
    ];
    if !config.hosted_web_search {
        specs.push(ToolSpec {
            name: "web_search",
            description: "搜索实时网页，返回标题、链接与摘要。凡是依赖最新信息或需要外部佐证的问题都应先搜索，再用 web_fetch 打开最相关的结果核实。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "搜索关键词"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        });
    }
    specs
}

/// Chat Completions 侧的工具定义。
///
/// 不要在带工具的请求上附加 `reasoning_effort` 之类的端点相关扩展：工具调用是
/// 跨模型的公共能力，而推理参数在不同中转上的支持并不一致。
pub(crate) fn chat_tool_definitions(
    config: HarnessConfig,
) -> Vec<async_openai::types::chat::ChatCompletionTools> {
    use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
    tool_specs(config)
        .into_iter()
        .map(|spec| {
            ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name: spec.name.to_string(),
                    description: Some(spec.description.to_string()),
                    parameters: Some(spec.parameters),
                    // 中转 API 对 strict 的支持不一致，JSON Schema 仍会正常约束参数。
                    strict: None,
                },
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FetchArgs {
    url: String,
    #[serde(default)]
    max_chars: Option<usize>,
}

pub(crate) async fn execute_tool(name: &str, arguments: &str, config: HarnessConfig) -> ToolRun {
    let started = Instant::now();
    let arguments = if arguments.trim().is_empty() {
        "{}"
    } else {
        arguments
    };
    let (result, label) = match name {
        "shell" => match serde_json::from_str::<ShellArgs>(arguments) {
            Ok(args) => {
                let label = super::search::truncate_chars(args.command.trim(), 60);
                (execute_shell(args, config).await, label)
            }
            Err(error) => (
                Err(anyhow::anyhow!("shell 参数无效：{error}")),
                String::new(),
            ),
        },
        "web_search" => match serde_json::from_str::<SearchArgs>(arguments) {
            Ok(args) => {
                let label = super::search::truncate_chars(args.query.trim(), 60);
                (execute_web_search(args, config).await, label)
            }
            Err(error) => (
                Err(anyhow::anyhow!("web_search 参数无效：{error}")),
                String::new(),
            ),
        },
        "web_fetch" => match serde_json::from_str::<FetchArgs>(arguments) {
            Ok(args) => {
                let label = super::search::truncate_chars(args.url.trim(), 60);
                (execute_web_fetch(args, config).await, label)
            }
            Err(error) => (
                Err(anyhow::anyhow!("web_fetch 参数无效：{error}")),
                String::new(),
            ),
        },
        other => (Err(anyhow::anyhow!("未知工具：{other}")), String::new()),
    };

    let elapsed = started.elapsed();
    let detail = if label.is_empty() {
        String::new()
    } else {
        format!(" {label}")
    };
    match result {
        Ok(output) => ToolRun {
            output,
            summary: format!("{name}{detail} · {:.1}s", elapsed.as_secs_f32()),
            failed: false,
        },
        Err(error) => ToolRun {
            output: format!("Tool error: {error:#}"),
            summary: format!("{name}{detail} · 失败"),
            failed: true,
        },
    }
}

// ---------------------------------------------------------------- shell

async fn execute_shell(args: ShellArgs, config: HarnessConfig) -> anyhow::Result<String> {
    if args.command.trim().is_empty() {
        anyhow::bail!("命令不能为空");
    }

    let timeout_seconds = args
        .timeout_seconds
        .unwrap_or(config.shell_timeout_seconds)
        .clamp(1, config.shell_timeout_seconds);
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "sh".to_string());
    let mut command = Command::new(shell);
    command
        .arg("-lc")
        .arg(&args.command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = args.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout 不可用"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("stderr 不可用"))?;
    let max_bytes = config.shell_max_output_bytes;
    let stdout_task = tokio::spawn(read_capped(stdout, max_bytes));
    let stderr_task = tokio::spawn(read_capped(stderr, max_bytes));

    let (status, timed_out) =
        match tokio::time::timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
            Ok(status) => (Some(status?), false),
            Err(_) => {
                let _ = child.kill().await;
                let status = child.wait().await.ok();
                (status, true)
            }
        };

    let pipes = tokio::time::timeout(Duration::from_secs(3), async {
        let (stdout, stderr) = tokio::join!(stdout_task, stderr_task);
        Ok::<_, anyhow::Error>((stdout??, stderr??))
    })
    .await;
    let (stdout, stderr) = match pipes {
        Ok(output) => output?,
        Err(_) => (Vec::new(), b"[output pipes did not close]\n".to_vec()),
    };

    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str("stdout:\n");
        output.push_str(&String::from_utf8_lossy(&stdout));
    }
    if !stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("stderr:\n");
        output.push_str(&String::from_utf8_lossy(&stderr));
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if timed_out {
        output.push_str(&format!("timed out after {timeout_seconds}s"));
    } else {
        output.push_str(&format!(
            "exit_code: {}",
            status.and_then(|status| status.code()).unwrap_or(-1)
        ));
    }
    Ok(truncate_utf8(output, max_bytes))
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut kept = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(kept)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let suffix = "\n[output truncated]";
    let keep = max_bytes.saturating_sub(suffix.len());
    let mut boundary = keep.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(suffix);
    value
}

// ------------------------------------------------------------- web 工具

async fn execute_web_search(args: SearchArgs, config: HarnessConfig) -> anyhow::Result<String> {
    let limit = args.limit.unwrap_or(config.web_search_results).clamp(1, 20);
    let outcome = super::search::search_web(&args.query, limit).await?;
    let mut output = format!(
        "「{}」的搜索结果（引擎：{}）：\n",
        args.query.trim(),
        outcome.engines.join("、")
    );
    for (index, hit) in outcome.hits.iter().enumerate() {
        output.push_str(&format!("\n[{}] {}\n    {}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            output.push_str(&format!("    {}\n", hit.snippet));
        }
    }
    output.push_str("\n提示：摘要可能过时或断章取义，落实到具体结论前请用 web_fetch 打开对应链接核对。");
    Ok(output)
}

async fn execute_web_fetch(args: FetchArgs, config: HarnessConfig) -> anyhow::Result<String> {
    let max_chars = args
        .max_chars
        .unwrap_or(config.web_fetch_max_chars)
        .clamp(500, 40_000);
    let page = super::webfetch::fetch_page(
        &args.url,
        max_chars,
        Duration::from_secs(config.web_fetch_timeout_seconds),
    )
    .await?;
    let mut output = String::new();
    if !page.title.is_empty() {
        output.push_str(&format!("标题：{}\n", page.title));
    }
    output.push_str(&format!("地址：{}\n", page.final_url));
    if !page.content_type.is_empty() {
        output.push_str(&format!("类型：{}\n", page.content_type));
    }
    output.push_str("---\n");
    output.push_str(&page.text);
    if page.truncated {
        output.push_str("\n\n[正文已按上限截断，如需后续内容请提高 max_chars 或换更具体的来源]");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> HarnessConfig {
        HarnessConfig {
            shell_timeout_seconds: 5,
            shell_max_output_bytes: 4096,
            web_search_results: 5,
            web_fetch_max_chars: 4000,
            web_fetch_timeout_seconds: 20,
            hosted_web_search: false,
        }
    }

    #[test]
    fn hosted_search_replaces_the_local_search_tool() {
        let names: Vec<&str> = tool_specs(test_config())
            .iter()
            .map(|spec| spec.name)
            .collect();
        assert!(names.contains(&"web_search"));

        let hosted = HarnessConfig {
            hosted_web_search: true,
            ..test_config()
        };
        let names: Vec<&str> = tool_specs(hosted).iter().map(|spec| spec.name).collect();
        assert!(!names.contains(&"web_search"));
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"shell"));
    }

    #[test]
    fn truncation_preserves_utf8_boundary() {
        let output = truncate_utf8("中文abcdef".to_string(), 10);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("truncated"));
    }

    #[tokio::test]
    async fn shell_returns_stdout_and_status() {
        let run = execute_tool("shell", r#"{"command":"printf harness-ok"}"#, test_config()).await;
        assert!(run.output.contains("harness-ok"));
        assert!(run.output.contains("exit_code: 0"));
        assert!(!run.failed);
        assert!(run.summary.starts_with("shell printf harness-ok"));
    }

    #[tokio::test]
    async fn invalid_arguments_are_reported_as_tool_errors() {
        let run = execute_tool("web_fetch", "{}", test_config()).await;
        assert!(run.failed);
        assert!(run.output.starts_with("Tool error:"), "{}", run.output);
    }

    #[tokio::test]
    async fn independent_tool_calls_can_run_together() {
        let config = test_config();
        let calls = [
            execute_tool("shell", r#"{"command":"printf one"}"#, config),
            execute_tool("shell", r#"{"command":"printf two"}"#, config),
        ];
        let output = futures_util::future::join_all(calls).await;
        assert!(output[0].output.contains("one"));
        assert!(output[1].output.contains("two"));
    }
}
