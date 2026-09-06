//! Markdown → 回复卡片图片。
//!
//! 两处改造：
//!
//! **速度**——旧流程是「设视口 → 注入 → 睡 200ms → 量高度 → 再设视口 → 睡 100ms
//! → 查元素 → 取盒模型 → 截图」，两次固定睡眠与多次 CDP 往返都花在等一个本来
//! 可以被观测到的状态上。现在只保留「设一次视口 → 注入 → 一次 evaluate（等字体
//! 就绪并量出卡片盒子）→ 带 clip 的整页截图」，睡眠与重复往返一并去掉。
//!
//! **可读性**——排版按聊天里「一屏读完」来调：正文行高放宽、层级用色块而非纯字号
//! 区分、代码块深色高对比、表格斑马纹、长 URL 强制断行；正文之外还能挂来源列表
//! 与耗时页脚，让读者一眼看清结论出处与代价。

use cdp_html_shot::{Browser, CaptureOptions, ClipRegion, ImageFormat, Viewport};
use pulldown_cmark::{Options, Parser, html};
use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;

/// 卡片 CSS 宽度；配合 2 倍像素密度即 1040px 位图，在手机聊天窗口里既清晰又不糊。
const CARD_WIDTH: u32 = 520;
/// 视口留出的左右留白。
const VIEWPORT_WIDTH: u32 = CARD_WIDTH + 40;
const DEVICE_SCALE: f64 = 2.0;
/// 单张图片的高度上限（CSS 像素），超出部分裁掉，避免生成超大图拖垮发送。
const MAX_CARD_HEIGHT: f64 = 20_000.0;
/// 整个渲染流程的上限，含浏览器获取与建标签页。
///
/// CDP 的每条命令都没有自带超时，浏览器一旦起不来或卡住就是永久阻塞——那意味着
/// 用户等到的不是一张丑图，而是彻底没有回复。兜这一道底，渲染失败就退化成纯文本，
/// 消息一定发得出去。
const RENDER_TIMEOUT: Duration = Duration::from_secs(45);
/// 等待网页字体就绪的上限。字体没到位会让 CJK 行高算错、卡片底部被切。
const FONT_WAIT_MS: u32 = 800;

/// 卡片内容。
pub(crate) struct Card<'a> {
    pub title: &'a str,
    pub markdown: &'a str,
    /// 参考来源，渲染成正文后的编号列表。
    pub sources: &'a [super::agent::Source],
    /// 页脚小字，例如模型、耗时与工具轨迹。
    pub footer: Option<String>,
}

/// 渲染成 base64 JPEG。
pub(crate) async fn render_card(card: Card<'_>) -> anyhow::Result<String> {
    let html = build_html(&card);
    match tokio::time::timeout(RENDER_TIMEOUT, render_html(&html)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "卡片渲染超过 {} 秒未完成（浏览器无响应）",
            RENDER_TIMEOUT.as_secs()
        )),
    }
}

async fn render_html(html: &str) -> anyhow::Result<String> {
    let browser = Browser::instance().await;
    let tab = browser.new_tab().await?;
    let result = capture(&tab, html).await;
    let _ = tab.close().await;
    result
}

async fn capture(tab: &cdp_html_shot::Tab, html: &str) -> anyhow::Result<String> {
    // 视口只设一次；最终尺寸由截图的 clip 决定，所以初始高度给个占位值即可。
    tab.set_viewport(&Viewport::new(VIEWPORT_WIDTH, 800).with_device_scale_factor(DEVICE_SCALE))
        .await?;
    tab.set_content(html).await?;

    // 等字体真正可用再量尺寸：CJK 字体换算前后的行高差别足以让卡片底部被切掉。
    //
    // 但每一步都必须有界。`Runtime.evaluate` 带 `awaitPromise` 时，promise 不落地
    // 就是永久挂起，而 headless 下 `requestAnimationFrame` 并不保证会触发——用它
    // 等布局提交，等来的往往是死锁。这里改用超时兜底的 `document.fonts.ready`
    // 加一次宏任务让位，正常情况下毫秒级返回。
    let measured = tab
        .evaluate(&format!(
            r#"(async () => {{
                const deadline = new Promise(resolve => setTimeout(resolve, {FONT_WAIT_MS}));
                try {{ await Promise.race([document.fonts.ready, deadline]); }} catch (_) {{}}
                await new Promise(resolve => setTimeout(resolve, 0));
                const card = document.querySelector('.card');
                if (!card) return null;
                const box = card.getBoundingClientRect();
                return {{
                    x: box.left + window.scrollX,
                    y: box.top + window.scrollY,
                    width: box.width,
                    height: box.height,
                }};
            }})()"#
        ))
        .await?;

    let number = |key: &str| measured.get(key).and_then(|value| value.as_f64());
    let width = number("width").filter(|value| *value > 1.0).unwrap_or(f64::from(CARD_WIDTH));
    let height = number("height")
        .filter(|value| *value > 1.0)
        .unwrap_or(800.0)
        .min(MAX_CARD_HEIGHT);

    let clip = ClipRegion::new(
        number("x").unwrap_or(0.0),
        number("y").unwrap_or(0.0),
        width,
        height,
    );
    let base64 = tab
        .screenshot(
            CaptureOptions::new()
                .with_format(ImageFormat::Jpeg)
                .with_quality(88)
                // 卡片通常远高于视口，必须允许越界捕获，否则底部会是空白。
                .with_full_page(true)
                .with_clip(clip),
        )
        .await?;
    Ok(base64)
}

fn build_html(card: &Card<'_>) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(card.markdown, options);
    let mut body = String::new();
    html::push_html(&mut body, parser);
    let body = label_code_blocks(&body);

    let sources = render_sources(card.sources);
    let footer = card
        .footer
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(r#"<div class="foot">{}</div>"#, escape_html(value)))
        .unwrap_or_default();

    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><style>{CSS}</style></head>
<body><div class="card"><div class="head">{title}</div><div class="body">{body}</div>{sources}{footer}</div></body></html>"#,
        title = escape_html(card.title),
    )
}

/// 把 `<pre><code class="language-x">` 改写成 `<pre data-lang="x">`，
/// 让 CSS 能用 `attr()` 在代码块角上标出语言——纯 CSS 拿不到子元素的类名。
fn label_code_blocks(html: &str) -> String {
    static CODE: OnceLock<Regex> = OnceLock::new();
    CODE.get_or_init(|| {
        Regex::new(r#"(?is)<pre><code class="language-([^"]+)">"#).unwrap()
    })
    .replace_all(html, |caps: &regex::Captures| {
        format!(r#"<pre data-lang="{}"><code>"#, escape_html(&caps[1]))
    })
    .into_owned()
}

fn render_sources(sources: &[super::agent::Source]) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let items = sources
        .iter()
        .take(12)
        .enumerate()
        .map(|(index, source)| {
            format!(
                r#"<li><span class="src-idx">{}</span><span class="src-title">{}</span><span class="src-host">{}</span></li>"#,
                index + 1,
                escape_html(&super::search::truncate_chars(&source.title, 48)),
                escape_html(&host_of(&source.url)),
            )
        })
        .collect::<String>();
    format!(r#"<div class="sources"><div class="src-head">参考来源</div><ol>{items}</ol></div>"#)
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.trim_start_matches("www.").to_string()))
        .unwrap_or_else(|| super::search::truncate_chars(url, 40))
}

pub(crate) fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const CSS: &str = r#"
*{box-sizing:border-box;margin:0;padding:0}
body{background:#eef1f5;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Noto Sans CJK SC","Source Han Sans SC","Hiragino Sans GB","Microsoft YaHei",Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased;padding:20px}
.card{width:520px;background:#fff;border-radius:14px;overflow:hidden;box-shadow:0 2px 14px rgba(15,23,42,.08)}
.head{padding:12px 18px;background:linear-gradient(135deg,#f8fafc,#eef2f7);border-bottom:1px solid #e6eaf0;font-size:12.5px;font-weight:600;color:#64748b;letter-spacing:.02em}
.body{padding:16px 18px 18px;font-size:15px;line-height:1.78;color:#24292f;word-wrap:break-word;overflow-wrap:anywhere}
.body>*:first-child{margin-top:0}
.body>*:last-child{margin-bottom:0}
p{margin:10px 0}
h1,h2,h3,h4{margin:20px 0 10px;font-weight:650;line-height:1.45;color:#0f172a}
h1{font-size:19px;padding-bottom:8px;border-bottom:2px solid #eef1f5}
h2{font-size:17px;padding-left:9px;border-left:4px solid #3b82f6}
h3{font-size:15.5px;color:#1e293b}
h4{font-size:15px;color:#334155}
ul,ol{margin:10px 0;padding-left:22px}
li{margin:5px 0}
li>p{margin:4px 0}
li::marker{color:#94a3b8}
input[type=checkbox]{margin-right:6px;accent-color:#3b82f6}
strong{font-weight:650;color:#0f172a}
em{color:#334155}
del{color:#94a3b8}
a{color:#2563eb;text-decoration:none;border-bottom:1px solid #bfdbfe}
code{padding:1.5px 5px;background:#f1f5f9;border-radius:5px;font-family:"SF Mono",Consolas,"Liberation Mono",Menlo,monospace;font-size:13px;color:#be185d}
pre{position:relative;margin:12px 0;padding:13px 14px;background:#161b22;border-radius:9px;overflow-x:auto}
pre[data-lang]{padding-top:26px}
pre[data-lang]::before{content:attr(data-lang);position:absolute;top:6px;left:14px;font-size:10.5px;letter-spacing:.06em;text-transform:uppercase;color:#7d8590}
pre code{display:block;padding:0;background:none;color:#e6edf3;font-size:12.5px;line-height:1.62;white-space:pre-wrap;word-break:break-word}
blockquote{margin:12px 0;padding:8px 12px;background:#f8fafc;border-left:3px solid #cbd5e1;border-radius:0 6px 6px 0;color:#475569}
blockquote p{margin:4px 0}
table{width:100%;margin:12px 0;border-collapse:collapse;font-size:13px;display:block;overflow-x:auto}
th,td{padding:7px 10px;border:1px solid #e2e8f0;text-align:left}
th{background:#f1f5f9;font-weight:650;color:#334155;white-space:nowrap}
tr:nth-child(2n) td{background:#fafbfc}
hr{margin:16px 0;border:none;border-top:1px solid #eef1f5}
img{max-width:100%;height:auto;margin:8px 0;border-radius:8px}
.footnote-definition{margin:6px 0;font-size:12.5px;color:#64748b}
.footnote-definition p{display:inline;margin:0}
.sources{padding:12px 18px;background:#f8fafc;border-top:1px solid #eef1f5}
.src-head{font-size:11.5px;font-weight:650;color:#94a3b8;letter-spacing:.08em;margin-bottom:7px}
.sources ol{list-style:none;padding:0;margin:0}
.sources li{display:flex;align-items:baseline;gap:7px;margin:4px 0;font-size:12.5px;line-height:1.5}
.src-idx{flex:none;min-width:17px;height:17px;border-radius:5px;background:#e0e7ff;color:#4338ca;font-size:10px;font-weight:700;display:flex;align-items:center;justify-content:center}
.src-title{color:#334155;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.src-host{flex:none;margin-left:auto;color:#94a3b8;font-size:11.5px}
.foot{padding:9px 18px;background:#f8fafc;border-top:1px solid #eef1f5;font-size:11px;color:#94a3b8;line-height:1.6;word-break:break-word}
/* 智能体与模型清单用的紧凑卡片；这些片段以裸 HTML 形式嵌在 Markdown 里。 */
.agent-card{margin:10px 0;padding:12px;background:#f8fafc;border:1px solid #eef1f5;border-radius:9px}
.agent-name{margin-bottom:7px;font-size:15px;font-weight:650;color:#0f172a}
.agent-info{font-size:12.5px;line-height:1.85;color:#64748b}
.agent-info code{font-size:11.5px}
.model-group{margin-bottom:15px;break-inside:avoid}
.model-header{display:flex;align-items:center;justify-content:space-between;margin-bottom:8px;padding:6px 10px;border-left:3px solid #3b82f6;border-radius:6px;background:#f1f5f9;font-size:12.5px;font-weight:650;color:#334155}
.model-count{padding:1px 6px;border-radius:4px;background:#e2e8f0;color:#64748b;font-size:10.5px}
.agent-grid{display:grid;grid-template-columns:repeat(2,1fr);gap:7px}
.agent-mini{padding:8px;border:1px solid #eef1f5;border-radius:7px;background:#fff}
.agent-mini-top{display:flex;align-items:center;margin-bottom:3px}
.agent-idx{flex:none;display:flex;align-items:center;justify-content:center;min-width:18px;height:18px;margin-right:6px;border-radius:5px;background:#e0e7ff;color:#4338ca;font-size:10px;font-weight:700}
.agent-mini-name{overflow:hidden;white-space:nowrap;text-overflow:ellipsis;font-size:13.5px;font-weight:600;color:#1e293b}
.agent-mini-desc{overflow:hidden;white-space:nowrap;text-overflow:ellipsis;font-size:11px;color:#94a3b8}
.mod-group{margin-bottom:15px;break-inside:avoid}
.mod-title{margin-bottom:8px;padding-left:7px;border-left:3px solid #3b82f6;font-size:12.5px;font-weight:700;letter-spacing:.05em;color:#475569;text-transform:uppercase}
.chip-box,.chip-container{display:flex;flex-wrap:wrap;gap:7px}
.chip{display:flex;align-items:center;padding:5px 9px;border:1px solid #e2e8f0;border-radius:7px;background:#fff;font-size:12.5px;color:#334155}
.chip-idx{margin-right:7px;padding:1px 5px;border-radius:4px;background:#f1f5f9;color:#94a3b8;font-family:"SF Mono",Consolas,monospace;font-size:10.5px;font-weight:650}
.chip-name{font-weight:500}
.chip-bad,.chip-badge{margin-left:7px;padding:1px 6px;border-radius:9px;background:#e0e7ff;color:#4338ca;font-size:10px;font-weight:650}
.provider-section{margin-bottom:18px;break-inside:avoid}
.provider-title{margin-bottom:8px;padding-left:6px;border-left:3px solid #94a3b8;font-size:13px;font-weight:700;color:#475569}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::oai::agent::Source;

    #[test]
    fn code_blocks_carry_their_language_label() {
        let html = label_code_blocks(r#"<pre><code class="language-rust">fn main(){}</code></pre>"#);
        assert_eq!(
            html,
            r#"<pre data-lang="rust"><code>fn main(){}</code></pre>"#
        );
    }

    #[test]
    fn renders_markdown_structure_and_sources() {
        let sources = [Source {
            title: "OpenAI 官网".into(),
            url: "https://www.openai.com/index/a?utm=1".into(),
        }];
        let html = build_html(&Card {
            title: "pi #3回复",
            markdown: "## 结论\n\n- 要点\n\n```rust\nfn main() {}\n```\n",
            sources: &sources,
            footer: Some("gpt-5.6-luna · 8.2秒".into()),
        });
        assert!(html.contains("<h2>结论</h2>"), "{html}");
        assert!(html.contains(r#"<pre data-lang="rust">"#), "{html}");
        assert!(html.contains("openai.com"), "{html}");
        assert!(!html.contains("utm=1"), "来源只展示域名");
        assert!(html.contains("gpt-5.6-luna · 8.2秒"), "{html}");
    }

    #[test]
    fn title_and_footer_are_escaped() {
        let html = build_html(&Card {
            title: "<script>x</script>",
            markdown: "hi",
            sources: &[],
            footer: Some("a & b".into()),
        });
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b"));
    }

    #[test]
    fn sources_block_is_omitted_when_empty() {
        assert!(render_sources(&[]).is_empty());
    }

    #[test]
    fn host_falls_back_to_the_raw_value() {
        assert_eq!(host_of("https://www.example.com/a"), "example.com");
        assert_eq!(host_of("not a url"), "not a url");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::plugins::oai::agent::Source;

    /// 真跑一次浏览器截图，确认卡片被完整量到（宽度按 2 倍像素密度出图，
    /// 高度不该退化成占位视口高度）。
    #[tokio::test]
    #[ignore = "需要本地 Chrome/Chromium"]
    async fn renders_a_complete_card() {
        let markdown = "## 标题\n\n正文一段，包含 `行内代码` 与 [链接](https://example.com)。\n\n\
                        - 第一点\n- 第二点\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n\
                        | 列 A | 列 B |\n| --- | --- |\n| 1 | 2 |\n";
        let sources = [Source {
            title: "示例来源".into(),
            url: "https://example.com/a".into(),
        }];
        let base64 = render_card(Card {
            title: "pi #1回复",
            markdown,
            sources: &sources,
            footer: Some("gpt-5.6-luna · 3.4秒 · web_search 测试".into()),
        })
        .await
        .unwrap();

        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&base64)
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!(
            image.width(),
            (f64::from(CARD_WIDTH) * DEVICE_SCALE) as u32
        );
        // 占位视口是 800，真实卡片必须比它高出一截才说明测量生效。
        assert!(image.height() > 900, "height = {}", image.height());
        std::fs::write("/tmp/ayjx-card.jpg", &bytes).ok();
    }
}
