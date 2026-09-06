//! `pi` 房间使用的轻量工具执行层。
//!
//! 结构沿用 pi-agent / oh-my-pi 的工具循环约定：模型收到 JSON Schema 工具定义，
//! 每轮的 tool call 在本机执行，结果以对应的 call id 回灌，直到模型给出最终文本。

use async_openai::types::chat::{
    ChatCompletionTool, ChatCompletionTools, FunctionObject,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

pub(crate) const MAX_TOOL_ROUNDS: usize = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HarnessConfig {
    pub shell_timeout_seconds: u64,
    pub shell_max_output_bytes: usize,
    pub web_search_results: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

pub(crate) fn tool_definitions() -> Vec<ChatCompletionTools> {
    vec![
        function_tool(
            "shell",
            "Execute a shell command on the host with full permissions. No user approval is required. Use this for terminal commands, files, programs, and system inspection.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Complete shell command to execute"},
                    "cwd": {"type": "string", "description": "Optional working directory"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 3600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        function_tool(
            "web_search",
            "Search the live web. Returns current result titles, URLs, and snippets. Use it whenever the answer depends on recent or externally verifiable information.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Web search query"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn function_tool(name: &str, description: &str, parameters: Value) -> ChatCompletionTools {
    ChatCompletionTools::Function(ChatCompletionTool {
        function: FunctionObject {
            name: name.to_string(),
            description: Some(description.to_string()),
            parameters: Some(parameters),
            // 中转 API 对 strict 的支持不一致，JSON Schema 仍会正常约束参数。
            strict: None,
        },
    })
}

pub(crate) async fn execute_tool(
    name: &str,
    arguments: &str,
    config: HarnessConfig,
) -> String {
    let result = match name {
        "shell" => match serde_json::from_str::<ShellArgs>(arguments) {
            Ok(args) => execute_shell(args, config).await,
            Err(error) => Err(anyhow::anyhow!("invalid shell arguments: {error}")),
        },
        "web_search" => match serde_json::from_str::<SearchArgs>(arguments) {
            Ok(args) => execute_web_search(args, config).await,
            Err(error) => Err(anyhow::anyhow!("invalid web_search arguments: {error}")),
        },
        _ => Err(anyhow::anyhow!("unknown tool: {name}")),
    };

    match result {
        Ok(output) => output,
        Err(error) => format!("Tool error: {error:#}"),
    }
}

async fn execute_shell(args: ShellArgs, config: HarnessConfig) -> anyhow::Result<String> {
    if args.command.trim().is_empty() {
        anyhow::bail!("command must not be empty");
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
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("stdout unavailable"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("stderr unavailable"))?;
    let max_bytes = config.shell_max_output_bytes;
    let stdout_task = tokio::spawn(read_capped(stdout, max_bytes));
    let stderr_task = tokio::spawn(read_capped(stderr, max_bytes));

    let (status, timed_out) = match tokio::time::timeout(
        Duration::from_secs(timeout_seconds),
        child.wait(),
    )
    .await
    {
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

async fn execute_web_search(args: SearchArgs, config: HarnessConfig) -> anyhow::Result<String> {
    let query = args.query.trim();
    if query.is_empty() {
        anyhow::bail!("query must not be empty");
    }
    let limit = args.limit.unwrap_or(config.web_search_results).clamp(1, 20);
    let results = match search_duckduckgo(query, limit).await {
        Ok(results) if !results.is_empty() => results,
        duckduckgo => match search_bing_rss(query, limit).await {
            Ok(results) if !results.is_empty() => results,
            bing => anyhow::bail!(
                "all public search providers failed (DuckDuckGo: {}; Bing: {})",
                search_error(duckduckgo),
                search_error(bing)
            ),
        },
    };

    let lines = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let snippet = if result.snippet.is_empty() {
                String::new()
            } else {
                format!("\n    {}", result.snippet)
            };
            format!("[{}] {}\n    {}{}", index + 1, result.title, result.url, snippet)
        })
        .collect::<Vec<_>>();
    Ok(format!(
        "Search results for {query:?}:\n{}",
        lines.join("\n\n")
    ))
}

fn search_error(result: anyhow::Result<Vec<SearchResult>>) -> String {
    match result {
        Ok(_) => "no results".to_string(),
        Err(error) => format!("{error:#}"),
    }
}

async fn search_duckduckgo(query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .append_pair("kl", "wt-wt")
        .append_pair("b", "")
        .finish();
    let response = crate::http::client()
        .post("https://html.duckduckgo.com/html/")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/124 Safari/537.36",
        )
        .body(form)
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("DuckDuckGo returned HTTP {status}");
    }
    if body.len() > 2 * 1024 * 1024 {
        anyhow::bail!("DuckDuckGo response exceeded 2 MiB");
    }
    let html = String::from_utf8_lossy(&body);
    if html.contains("anomaly-modal") || html.contains("anomaly.js") {
        anyhow::bail!("DuckDuckGo rejected the automated search request");
    }
    Ok(parse_search_results(&html, limit))
}

async fn search_bing_rss(query: &str, limit: usize) -> anyhow::Result<Vec<SearchResult>> {
    let mut url = url::Url::parse("https://www.bing.com/search")?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "rss")
        .append_pair("count", &limit.to_string());
    let response = crate::http::client()
        .get(url)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("Bing returned HTTP {status}");
    }
    if body.len() > 2 * 1024 * 1024 {
        anyhow::bail!("Bing response exceeded 2 MiB");
    }
    Ok(parse_bing_rss(&String::from_utf8_lossy(&body), limit))
}

fn parse_bing_rss(xml: &str, limit: usize) -> Vec<SearchResult> {
    static ITEM: OnceLock<Regex> = OnceLock::new();
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM.get_or_init(|| Regex::new(r"(?is)<item>(.*?)</item>").unwrap());
    let field_re = FIELD.get_or_init(|| {
        Regex::new(r"(?is)<(title|link|description)>(.*?)</(?:title|link|description)>").unwrap()
    });
    item_re
        .captures_iter(xml)
        .filter_map(|item| {
            let mut title = String::new();
            let mut url = String::new();
            let mut snippet = String::new();
            for field in field_re.captures_iter(item.get(1)?.as_str()) {
                let value = decode_html(field.get(2)?.as_str());
                match field.get(1)?.as_str().to_ascii_lowercase().as_str() {
                    "title" => title = value,
                    "link" => url = value,
                    "description" => snippet = value,
                    _ => {}
                }
            }
            if title.is_empty()
                || !(url.starts_with("https://") || url.starts_with("http://"))
            {
                None
            } else {
                Some(SearchResult { title, url, snippet })
            }
        })
        .take(limit)
        .collect()
}

fn parse_search_results(html: &str, limit: usize) -> Vec<SearchResult> {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    let title_re = TITLE.get_or_init(|| {
        Regex::new(
            r#"(?is)<a\b[^>]*class=["'][^"']*\bresult__a\b[^"']*["'][^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#,
        )
        .unwrap()
    });
    let snippet_re = SNIPPET.get_or_init(|| {
        Regex::new(
            r#"(?is)<(?:a|div|span)\b[^>]*class=["'][^"']*\bresult__snippet\b[^"']*["'][^>]*>(.*?)</(?:a|div|span)>"#,
        )
        .unwrap()
    });
    let matches = title_re.captures_iter(html).collect::<Vec<_>>();
    let mut results = Vec::new();
    for (index, captures) in matches.iter().enumerate() {
        let Some(href) = captures.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let Some(url) = unwrap_result_url(href) else {
            continue;
        };
        if results.iter().any(|result: &SearchResult| result.url == url) {
            continue;
        }
        let title = decode_html(captures.get(2).map(|value| value.as_str()).unwrap_or(""));
        if title.is_empty() {
            continue;
        }
        let start = captures.get(0).map(|value| value.end()).unwrap_or(0);
        let end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map(|value| value.start())
            .unwrap_or(html.len());
        let snippet = snippet_re
            .captures(&html[start..end])
            .and_then(|value| value.get(1))
            .map(|value| decode_html(value.as_str()))
            .unwrap_or_default();
        results.push(SearchResult { title, url, snippet });
        if results.len() >= limit {
            break;
        }
    }
    results
}

fn decode_html(value: &str) -> String {
    static TAGS: OnceLock<Regex> = OnceLock::new();
    static SPACE: OnceLock<Regex> = OnceLock::new();
    // RSS description 常把片段标签编码成实体，先解码一次才能再正确去标签。
    let decoded_entities = quick_xml::escape::unescape(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_string());
    let without_tags = TAGS
        .get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap())
        .replace_all(&decoded_entities, " ");
    let decoded = quick_xml::escape::unescape(&without_tags)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| without_tags.into_owned());
    SPACE
        .get_or_init(|| Regex::new(r"\s+").unwrap())
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

fn unwrap_result_url(href: &str) -> Option<String> {
    let href = href.replace("&amp;", "&");
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href
    };
    if let Ok(url) = url::Url::parse(&absolute)
        && let Some((_, target)) = url.query_pairs().find(|(name, _)| name == "uddg")
    {
        return Some(target.into_owned());
    }
    if absolute.starts_with("https://") || absolute.starts_with("http://") {
        return Some(absolute);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_unwraps_search_results() {
        let html = r#"
          <div class="result results_links">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1&amp;rut=abc"><b>Example</b> &amp; result</a>
            <a class="result__snippet">A useful <b>search</b> result.</a>
          </div>
        "#;
        assert_eq!(
            parse_search_results(html, 5),
            vec![SearchResult {
                title: "Example & result".to_string(),
                url: "https://example.com/a?x=1".to_string(),
                snippet: "A useful search result.".to_string(),
            }]
        );
    }

    #[test]
    fn parses_bing_rss_results() {
        let xml = r#"<rss><channel><item><title>Rust &amp; tools</title><link>https://example.com/rust</link><description>Fast &lt;b&gt;agent&lt;/b&gt; runtime.</description></item></channel></rss>"#;
        assert_eq!(
            parse_bing_rss(xml, 5),
            vec![SearchResult {
                title: "Rust & tools".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: "Fast agent runtime.".to_string(),
            }]
        );
    }

    #[test]
    fn truncation_preserves_utf8_boundary() {
        let output = truncate_utf8("中文abcdef".to_string(), 10);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("truncated"));
    }

    #[tokio::test]
    async fn shell_returns_stdout_and_status() {
        let output = execute_tool(
            "shell",
            r#"{"command":"printf harness-ok"}"#,
            HarnessConfig {
                shell_timeout_seconds: 5,
                shell_max_output_bytes: 4096,
                web_search_results: 5,
            },
        )
        .await;
        assert!(output.contains("harness-ok"));
        assert!(output.contains("exit_code: 0"));
    }

    #[tokio::test]
    async fn independent_tool_calls_can_run_together() {
        let config = HarnessConfig {
            shell_timeout_seconds: 5,
            shell_max_output_bytes: 4096,
            web_search_results: 5,
        };
        let calls = [
            execute_tool("shell", r#"{"command":"printf one"}"#, config),
            execute_tool("shell", r#"{"command":"printf two"}"#, config),
        ];
        let output = futures_util::future::join_all(calls).await;
        assert!(output[0].contains("one"));
        assert!(output[1].contains("two"));
    }

    #[tokio::test]
    #[ignore = "需要访问公共搜索引擎"]
    async fn live_web_search_returns_links() {
        let output = execute_tool(
            "web_search",
            r#"{"query":"OpenAI official website","limit":3}"#,
            HarnessConfig {
                shell_timeout_seconds: 5,
                shell_max_output_bytes: 4096,
                web_search_results: 3,
            },
        )
        .await;
        assert!(output.contains("http"), "{output}");
        assert!(!output.starts_with("Tool error:"), "{output}");
    }
}
