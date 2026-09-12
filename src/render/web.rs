//! help / ctl 共用的网页阅读卡片。动态内容始终作为文本转义，不加载外部资源。

use anyhow::{Result, anyhow, ensure};
use cdp_html_shot::{Browser, CaptureOptions, ImageFormat, Viewport};
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, time::Duration};
use tokio::{sync::Semaphore, time::timeout};

pub enum Theme {
    Help,
    Control,
}
pub struct Item {
    pub name: String,
    pub key: String,
    pub desc: String,
    pub on: bool,
}
pub struct Cmd {
    pub prefix: String,
    pub cmd: String,
    pub note: String,
    pub aliases: Vec<String>,
}
pub struct Row {
    pub on: bool,
    pub main: String,
    pub sub: String,
    pub tail: String,
}
pub struct Tile {
    pub value: String,
    pub label: String,
}
pub enum Tone {
    Info,
    Empty,
}
pub enum Block {
    Title {
        title: String,
        pill: Option<(String, bool)>,
        sub: String,
    },
    Meter(Vec<bool>),
    Rule,
    Section {
        title: String,
        en: String,
        count: String,
    },
    Items(Vec<Item>),
    Cmds(Vec<Cmd>),
    Rows(Vec<Row>),
    Code(Vec<String>),
    Tiles(Vec<Tile>),
    Callout {
        tone: Tone,
        text: String,
    },
}
pub struct Doc {
    pub theme: Theme,
    pub width: f32,
    pub kicker: String,
    pub blocks: Vec<Block>,
    pub foot: String,
    pub hint: (String, String),
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn badge(label: &str, state: &str) -> String {
    format!(
        r#"<span class="badge {state}"><i></i>{}</span>"#,
        esc(label)
    )
}
fn status(on: bool) -> String {
    badge(
        if on { "已启用" } else { "已停用" },
        if on { "on" } else { "off" },
    )
}

pub fn html(doc: &Doc) -> String {
    let mut body = String::new();
    for block in &doc.blocks {
        match block {
            Block::Title { title, pill, sub } => {
                body.push_str(&format!("<header><div class=heading><h1>{}</h1>{}</div><p class=subtitle>{}</p></header>",
                    esc(title), pill.as_ref().map(|(label, on)| badge(label, if *on { "on" } else { "off" })).unwrap_or_default(), esc(sub)));
            }
            Block::Meter(states) => {
                let on = states.iter().filter(|s| **s).count();
                body.push_str(&format!("<div class=summary><span>全部 <b>{}</b></span><span>已启用 <b>{on}</b></span><span>已停用 <b>{}</b></span></div>", states.len(), states.len() - on));
            }
            Block::Rule => body.push_str("<hr>"),
            Block::Section { title, en, count } => body.push_str(&format!(
                "<div class=section><h2>{}</h2><span class=section-en>{}</span><span class=count>{}</span></div>", esc(title), esc(en), esc(count))),
            Block::Items(items) => {
                body.push_str("<div class=items>");
                for item in items {
                    body.push_str(&format!("<article class=item><div class=item-heading><h3>{}</h3>{}</div><div class=key>{}</div><p class=description>{}</p></article>",
                        esc(&item.name), status(item.on), esc(&item.key), esc(&item.desc)));
                }
                body.push_str("</div>");
            }
            Block::Cmds(cmds) => {
                body.push_str("<ol class=commands>");
                for cmd in cmds {
                    body.push_str(&format!("<li><code class=command>{}{}</code>", esc(&cmd.prefix), esc(&cmd.cmd)));
                    if !cmd.note.is_empty() { body.push_str(&format!("<p class=description>{}</p>", esc(&cmd.note))); }
                    if !cmd.aliases.is_empty() {
                        body.push_str("<div class=aliases><span>别名</span>");
                        for alias in &cmd.aliases { body.push_str(&format!("<code>{}</code>", esc(alias))); }
                        body.push_str("</div>");
                    }
                    body.push_str("</li>");
                }
                body.push_str("</ol>");
            }
            Block::Rows(rows) => {
                body.push_str("<div class=status-list>");
                for row in rows {
                    body.push_str(&format!("<div class=status-row><div class=identity><h3>{}</h3><div class=key>{}</div></div><div class=states>{}{}</div></div>",
                        esc(&row.main), esc(&row.sub), status(row.on),
                        if row.tail.is_empty() { String::new() } else { badge(&row.tail, "pending") }));
                }
                body.push_str("</div>");
            }
            Block::Code(lines) => {
                body.push_str("<div class=code-panel>");
                for line in lines {
                    let class = if line.trim().starts_with('[') { "code-line table-key" } else { "code-line" };
                    body.push_str(&format!("<div class=\"{class}\"><code>{}</code></div>", if line.is_empty() { "&#8203;".into() } else { esc(line) }));
                }
                body.push_str("</div>");
            }
            Block::Tiles(tiles) => {
                body.push_str("<div class=tiles>");
                for tile in tiles { body.push_str(&format!("<div><b>{}</b><span>{}</span></div>", esc(&tile.value), esc(&tile.label))); }
                body.push_str("</div>");
            }
            Block::Callout { tone, text } => body.push_str(&format!("<aside class=\"callout {}\">{}</aside>",
                match tone { Tone::Info => "info", Tone::Empty => "empty" }, esc(text))),
        }
    }
    let theme = match doc.theme {
        Theme::Help => "help",
        Theme::Control => "control",
    };
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; font-src data:">
<title>AYJX · {title}</title><style>{css}</style></head>
<body class="{theme}" style="width:{width}px"><main class="shot"><div class="card">
<div class="eyebrow"><span class="brand">AYJX</span><span>{kicker}</span></div>
{body}<footer><div class="next"><span>{hint}</span><code>{command}</code></div><p>{foot}</p></footer>
</div></main></body></html>"#,
        title = esc(&doc.kicker),
        css = include_str!("../../res/cards/reading.css"),
        width = doc.width,
        kicker = esc(&doc.kicker.replace("AYJX · ", "")),
        hint = esc(&doc.hint.0),
        command = esc(&doc.hint.1),
        foot = esc(&doc.foot)
    )
}

// 系统卡片串行截图，避免群聊同时请求时抢占大量内存。排队时间计入超时。
static CAPTURE_GATE: Semaphore = Semaphore::const_new(1);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

fn scale_factor(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        3.0
    }
}

pub async fn capture(doc: &Doc, scale: f64, browser_path: Option<&str>) -> Result<String> {
    capture_html(&html(doc), doc.width as u32, scale, browser_path).await
}

/// 把一段自带样式的整页 HTML 截成 PNG base64。
///
/// 与 [`capture`] 走同一道串行闸门与同一套尺寸护栏，只是版面由调用方自己写——
/// help / ctl 的 `Doc` 模型排不出来的卡片（如篇幅很长的插件手册）走这里。
/// 页面里必须有一个 `.shot` 元素，它的外接矩形就是出图范围。
pub async fn capture_html(
    html: &str,
    width: u32,
    scale: f64,
    browser_path: Option<&str>,
) -> Result<String> {
    let scale = scale_factor(scale);
    let mut page = None;
    // cdp-html-shot 的全局实例初始化失败会 panic，转换为可回退的普通错误。
    let result = AssertUnwindSafe(timeout(CAPTURE_TIMEOUT, async {
        let _permit = CAPTURE_GATE.acquire().await?;
        let browser = match browser_path.filter(|p| !p.is_empty()) {
            Some(path) => Browser::instance_with_path(path).await,
            None => Browser::instance().await,
        };
        page = Some(browser.new_tab().await?);
        let tab = page.as_ref().unwrap();
        tab.set_viewport(&Viewport::new(width, 600).with_device_scale_factor(scale)).await?;
        tab.set_content(html).await?;
        tab.evaluate("document.fonts.ready.then(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true)))))").await?;
        let height = tab.evaluate("Math.ceil(document.querySelector('.shot').getBoundingClientRect().height)").await?
            .as_f64().ok_or_else(|| anyhow!("无法测量卡片高度"))?;
        ensure!(height.is_finite() && height > 0.0 && height <= 16000.0
            && f64::from(width) * height * scale * scale <= 64_000_000.0,
            "卡片超出安全出图尺寸，改用完整文本");
        let viewport = Viewport::new(width, height as u32).with_device_scale_factor(scale);
        tab.set_viewport(&viewport).await?;
        let opts = CaptureOptions::new().with_viewport(viewport).with_format(ImageFormat::Png);
        tab.find_element(".shot").await?.screenshot_with_options(opts).await
    })).catch_unwind().await;
    // 成功、错误和超时均清理页面；不能在 timeout 的 ? 之后才安排清理。
    if let Some(tab) = page {
        let _ = timeout(Duration::from_secs(3), tab.close()).await;
    }
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(anyhow!("网页卡片截图超时（45 秒）")),
        Err(_) => Err(anyhow!("浏览器初始化失败")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_content_is_text_and_long_content_is_complete() {
        let payload = "<script>alert('x')</script> & \"配置\"";
        let long = "超长字段".repeat(500);
        let doc = Doc {
            theme: Theme::Control,
            width: 640.0,
            kicker: payload.into(),
            blocks: vec![
                Block::Code(vec![payload.into(), long.clone()]),
                Block::Cmds(vec![Cmd {
                    prefix: "&".into(),
                    cmd: "help <插件>".into(),
                    note: payload.into(),
                    aliases: vec![payload.into()],
                }]),
                Block::Rows(vec![Row {
                    on: false,
                    main: payload.into(),
                    sub: payload.into(),
                    tail: "待重启".into(),
                }]),
            ],
            foot: payload.into(),
            hint: (payload.into(), payload.into()),
        };
        let out = html(&doc);
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&amp;help &lt;插件&gt;"));
        assert!(out.contains(&long));
        assert!(out.contains("已停用") && out.contains("待重启"));
        assert!(out.contains("default-src 'none'"));
    }
    /// 用真实浏览器覆盖尺寸超限回退与失败后的下一次截图。
    #[tokio::test]
    #[ignore = "需要 Chromium；运行时与其他截图测试串行"]
    async fn oversized_card_fails_without_breaking_the_next_capture() {
        let mut doc = Doc {
            theme: Theme::Help,
            width: 640.0,
            kicker: "TEST".into(),
            blocks: vec![Block::Code(vec!["完整保留内容".into(); 600])],
            foot: "末尾".into(),
            hint: ("返回".into(), "/help".into()),
        };
        let path = std::env::var("CHROME_BIN").ok();
        let error = capture(&doc, 3.0, path.as_deref()).await.unwrap_err();
        assert!(error.to_string().contains("安全出图尺寸"), "{error}");
        doc.blocks = vec![Block::Code(vec!["正常卡片".into()])];
        let image = capture(&doc, 1.0, path.as_deref()).await.unwrap();
        assert!(image.starts_with("iVBOR"));
        Browser::shutdown_global().await;
        let error = capture(&doc, 1.0, Some("/nonexistent/ayjx-test-chromium"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("浏览器初始化失败"), "{error}");
    }

    #[test]
    fn scale_is_bounded_even_for_non_finite_values() {
        assert_eq!(scale_factor(f64::NAN), 3.0);
        assert_eq!(scale_factor(f64::INFINITY), 3.0);
        assert_eq!(scale_factor(0.0), 1.0);
        assert_eq!(scale_factor(9.0), 4.0);
    }
}
