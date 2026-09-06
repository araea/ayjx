//! 网页正文抓取：把一个 URL 变成模型能直接阅读的纯文本。
//!
//! 只有搜索摘要时，模型要么答得含糊，要么凭印象编造。pi-agent / oh-my-pi 的做法
//! 是搜索与读取分成两件工具：搜索给候选，读取给证据。这里实现的是读取那一半，
//! 不引入 DOM 库——正则去脚本样式、按块级标签还原换行，再折叠空白即可满足
//! 「给模型看」的精度要求，同时保持 Termux 上的编译体积与速度。

use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;

/// 抓取结果。`text` 已按 `max_chars` 截断。
#[derive(Debug, Clone)]
pub(crate) struct FetchedPage {
    pub title: String,
    pub final_url: String,
    pub content_type: String,
    pub text: String,
    pub truncated: bool,
}

/// 下载并抽取正文。非文本类型（图片、压缩包等）直接报错，避免把二进制塞进上下文。
pub(crate) async fn fetch_page(
    url: &str,
    max_chars: usize,
    timeout: Duration,
) -> anyhow::Result<FetchedPage> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|error| anyhow::anyhow!("无效的 URL：{error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("只支持 http/https，收到 {}", parsed.scheme());
    }

    let response = crate::http::client()
        .get(parsed)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
        )
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/json;q=0.9,text/plain;q=0.8,*/*;q=0.5",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
        .timeout(timeout)
        .send()
        .await?;

    let status = response.status();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if !status.is_success() {
        anyhow::bail!("HTTP {status}（{final_url}）");
    }
    if !is_textual(&mime) {
        anyhow::bail!("内容类型 {mime} 不是文本，无法阅读");
    }

    // 正文一般远小于原始 HTML，多留几倍余量再截断。
    let byte_budget = max_chars.saturating_mul(16).clamp(64 * 1024, 8 * 1024 * 1024);
    let body = response.bytes().await?;
    let raw = String::from_utf8_lossy(&body[..body.len().min(byte_budget)]);

    let (title, body_text) = if mime.contains("html") || mime.contains("xml") {
        let title = extract_title(&raw);
        (title, html_to_text(&raw))
    } else {
        (String::new(), collapse_blank_lines(&raw))
    };

    let (text, truncated) = truncate_chars(&body_text, max_chars);
    if text.trim().is_empty() {
        anyhow::bail!("页面没有可提取的文本正文（{final_url}）");
    }
    Ok(FetchedPage {
        title,
        final_url,
        content_type: mime,
        text,
        truncated,
    })
}

fn is_textual(mime: &str) -> bool {
    mime.is_empty()
        || mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
        || mime.contains("x-yaml")
}

fn extract_title(html: &str) -> String {
    static TITLE: OnceLock<Regex> = OnceLock::new();
    TITLE
        .get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap())
        .captures(html)
        .and_then(|caps| caps.get(1))
        .map(|value| super::search::decode_html(value.as_str()))
        .unwrap_or_default()
}

/// HTML → 纯文本。先剪掉非正文容器，再把块级标签换成换行，最后统一去标签。
///
/// 存在 `<article>` / `<main>` 时只取其内容：导航、侧栏、页脚对模型是纯噪声，
/// 挤占的正是上下文预算。
pub(crate) fn html_to_text(html: &str) -> String {
    static DROP: OnceLock<Regex> = OnceLock::new();
    static MAIN: OnceLock<Regex> = OnceLock::new();
    static HEADING: OnceLock<Regex> = OnceLock::new();
    static LIST_ITEM: OnceLock<Regex> = OnceLock::new();
    static BLOCK: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();

    // `regex` 不支持反向引用，配对标签只能逐个写全；这也顺带避免了
    // `<(a|b)>…</\1>` 那种模式在嵌套结构上的误配。
    let dropped = DROP
        .get_or_init(|| {
            const NOISE_TAGS: [&str; 11] = [
                "script", "style", "noscript", "template", "svg", "head", "nav", "footer",
                "form", "iframe", "aside",
            ];
            let pattern = NOISE_TAGS
                .iter()
                .map(|tag| format!(r"<{tag}\b[^>]*>.*?</{tag}>"))
                .collect::<Vec<_>>()
                .join("|");
            Regex::new(&format!("(?is){pattern}")).unwrap()
        })
        .replace_all(html, " ");

    let main = MAIN
        .get_or_init(|| {
            Regex::new(r"(?is)<article\b[^>]*>(.*?)</article>|<main\b[^>]*>(.*?)</main>").unwrap()
        })
        .captures(&dropped)
        .and_then(|caps| caps.get(1).or_else(|| caps.get(2)))
        .map(|value| value.as_str().to_string());
    let source = main.as_deref().unwrap_or(&dropped);

    let with_headings = HEADING
        .get_or_init(|| Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]>").unwrap())
        .replace_all(source, |caps: &regex::Captures| {
            let level = caps[1].parse::<usize>().unwrap_or(1).clamp(1, 6);
            format!("\n\n{} {}\n", "#".repeat(level), &caps[2])
        });
    let with_items = LIST_ITEM
        .get_or_init(|| Regex::new(r"(?is)<li\b[^>]*>").unwrap())
        .replace_all(&with_headings, "\n- ");
    let with_blocks = BLOCK
        .get_or_init(|| {
            Regex::new(r"(?is)</?(p|div|section|tr|br|ul|ol|table|blockquote|pre|h[1-6])\b[^>]*>")
                .unwrap()
        })
        .replace_all(&with_items, "\n");

    let text = TAG
        .get_or_init(|| Regex::new(r"(?is)<[^>]*>").unwrap())
        .replace_all(&with_blocks, "");
    let decoded = quick_xml::escape::unescape(&text)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| text.into_owned());
    collapse_blank_lines(&decoded)
}

/// 折叠行内多余空白和连续空行，保留段落结构。
fn collapse_blank_lines(value: &str) -> String {
    static INLINE_SPACE: OnceLock<Regex> = OnceLock::new();
    let inline = INLINE_SPACE.get_or_init(|| Regex::new(r"[ \t\u{00a0}]+").unwrap());
    let mut out = String::with_capacity(value.len() / 2);
    let mut blank_run = 0_usize;
    for line in value.lines() {
        let trimmed = inline.replace_all(line, " ").trim().to_string();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        out.push_str(&trimmed);
        out.push('\n');
    }
    out.trim().to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    for (count, ch) in value.chars().enumerate() {
        if count >= max_chars {
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_chrome_and_keeps_article_structure() {
        let html = r#"
          <html><head><title>Doc &amp; Title</title><style>.a{}</style></head>
          <body>
            <nav>Home About</nav>
            <article>
              <h2>Section</h2>
              <p>First paragraph.</p>
              <ul><li>alpha</li><li>beta</li></ul>
            </article>
            <footer>copyright</footer>
          </body></html>
        "#;
        assert_eq!(extract_title(html), "Doc & Title");
        let text = html_to_text(html);
        assert!(text.contains("## Section"), "{text}");
        assert!(text.contains("First paragraph."), "{text}");
        assert!(text.contains("- alpha"), "{text}");
        assert!(!text.contains("Home About"), "{text}");
        assert!(!text.contains("copyright"), "{text}");
    }

    #[test]
    fn truncation_reports_the_cut() {
        let (text, truncated) = truncate_chars("中文abcdef", 4);
        assert_eq!(text, "中文ab");
        assert!(truncated);
    }

    #[tokio::test]
    #[ignore = "需要访问公网"]
    async fn live_fetch_reads_a_page() {
        let page = fetch_page("https://example.com/", 2000, Duration::from_secs(20))
            .await
            .unwrap();
        assert!(page.text.contains("Example Domain"), "{page:?}");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// 真抓几个模型常引用的站点，确认正文抽取不会被导航和脚本淹没。
    #[tokio::test]
    #[ignore = "需要访问公网"]
    async fn reads_real_pages() {
        for url in [
            "https://blog.rust-lang.org/releases/",
            "https://doc.rust-lang.org/releases.html",
        ] {
            let page = fetch_page(url, 3000, Duration::from_secs(25))
                .await
                .unwrap_or_else(|error| panic!("{url}: {error:#}"));
            println!(
                "--- {url}\n标题：{}\n{}\n",
                page.title,
                page.text.chars().take(240).collect::<String>()
            );
            assert!(page.text.chars().count() > 200, "{url} 正文过短");
        }
    }
}
