use crate::adapters::onebot::{LockedWriter, api};
use crate::event::Context;
use cdp_html_shot::{Browser, CaptureOptions, Viewport};
use pulldown_cmark::{Options, Parser, html};
use regex::Regex;
use simd_json::base::ValueAsScalar;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time;

pub static RE_API: OnceLock<Regex> = OnceLock::new();
pub static RE_IDX: OnceLock<Regex> = OnceLock::new();

pub const MODEL_KEYWORDS: &[&str] = &[
    "gpt-5.5",
    "gpt-image",
    "claude-opus-4",
    "gemini-3.1",
    "deepseek-v4",
];

pub fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '！' => '!',
            '＠' => '@',
            '＃' => '#',
            '＄' => '$',
            '％' => '%',
            '＊' => '*',
            '（' => '(',
            '）' => ')',
            '－' => '-',
            '＋' => '+',
            '：' => ':',
            '；' => ';',
            '“' | '”' => '"',
            '‘' | '’' => '\'',
            '，' => ',',
            '。' => '.',
            '？' => '?',
            '～' => '~',
            '＿' => '_',
            '＆' => '&',
            '／' => '/',
            '＝' => '=',
            _ => c,
        })
        .collect()
}

pub fn parse_api(text: &str) -> Option<(String, String)> {
    let re = RE_API.get_or_init(|| {
        Regex::new(r"(?s)^(https?://\S+)\s+(sk-\S+)$|^(sk-\S+)\s+(https?://\S+)$").unwrap()
    });
    let t = text.trim();
    re.captures(t).and_then(|c| {
        c.get(1)
            .zip(c.get(2))
            .map(|(u, k)| (u.as_str().to_string(), k.as_str().to_string()))
            .or_else(|| {
                c.get(3)
                    .zip(c.get(4))
                    .map(|(k, u)| (u.as_str().to_string(), k.as_str().to_string()))
            })
    })
}

pub fn parse_indices(s: &str) -> Vec<usize> {
    let s = s.replace('，', ",");
    let re = RE_IDX.get_or_init(|| Regex::new(r"(\d+)(?:-(\d+))?").unwrap());
    let mut v = Vec::new();
    for c in re.captures_iter(&s) {
        if let Some(start) = c.get(1).and_then(|m| m.as_str().parse().ok()) {
            if let Some(end) = c.get(2).and_then(|m| m.as_str().parse().ok()) {
                v.extend(start..=end);
            } else {
                v.push(start);
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

pub fn filter_models(models: &[String]) -> Vec<String> {
    models
        .iter()
        .filter(|m| {
            let lower = m.to_lowercase();
            MODEL_KEYWORDS.iter().any(|kw| lower.contains(kw))
        })
        .cloned()
        .collect()
}

pub fn escape_markdown_special(s: &str) -> String {
    match serde_json::to_string(s) {
        Ok(escaped) => {
            let trimmed = escaped.trim_matches('"');
            trimmed.replace("\\n", "\n").replace("\\t", "\t")
        }
        Err(_) => s.to_string(),
    }
}

pub async fn render_md(md: &str, title: &str) -> anyhow::Result<String> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts);
    let mut html_body = String::new();
    html::push_html(&mut html_body, parser);

    let css = r#"
 *{box-sizing:border-box}
 body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Hiragino Sans GB","Microsoft YaHei",Helvetica,Arial,sans-serif;font-size:15px;line-height:1.6;background:#f5f5f5;color:#333;padding:0;margin:0}
 .md{background:#fff;padding:16px 14px;margin:0;max-width:480px;width:90vw;word-wrap:break-word;overflow-wrap:break-word}
 .title{font-size:13px;color:#888;border-bottom:1px solid #eee;padding-bottom:10px;margin-bottom:14px;font-weight:500}
 h1,h2,h3{margin:16px 0 10px;font-weight:600;line-height:1.4}
 h1{font-size:20px;border-bottom:2px solid #eee;padding-bottom:8px}
 h2{font-size:18px;border-bottom:1px solid #eee;padding-bottom:6px}
 h3{font-size:16px}
 p{margin:10px 0}
 table{border-collapse:collapse;margin:12px 0;width:100%;font-size:13px;display:block;overflow-x:auto}
 td,th{padding:8px 10px;border:1px solid #ddd;text-align:left}
 th{font-weight:600;background:#f8f9fa}
 tr:nth-child(2n){background:#fafafa}
 code{padding:2px 6px;background:#f0f0f0;border-radius:4px;font-family:"SF Mono",Consolas,"Liberation Mono",Menlo,monospace;font-size:13px;color:#d63384;white-space:pre-wrap;word-wrap:break-word;}
 pre{background:#f6f8fa;border-radius:8px;padding:12px;overflow-x:auto;margin:12px 0;white-space:pre-wrap;word-wrap:break-word;overflow-wrap: break-word;}
 pre code{background:none;padding:0;color:#333}
 blockquote{margin:12px 0;padding:8px 12px;color:#666;border-left:3px solid #ddd;background:#fafafa;border-radius:0 4px 4px 0}
 img{max-width:100%;height:auto;border-radius:6px;margin:8px 0}
 ul,ol{padding-left:20px;margin:10px 0}
 li{margin:4px 0}
 hr{border:none;border-top:1px solid #eee;margin:16px 0}
 a{color:#0066cc;text-decoration:none}
 strong{font-weight:600}
 .agent-card{background:#fafbfc;border:1px solid #e8e8e8;border-radius:8px;padding:12px;margin:10px 0}
 .agent-name{font-size:16px;font-weight:600;color:#333;margin-bottom:8px}
 .agent-info{font-size:13px;color:#666;line-height:1.8}
 .agent-info code{font-size:12px}
 .model-group{margin-bottom:16px;break-inside:avoid;}
 .model-header{background:#f0f2f5;color:#444;padding:6px 10px;border-radius:6px;font-weight:600;font-size:13px;margin-bottom:8px;display:flex;justify-content:space-between;align-items:center;border-left:3px solid #0066cc;}
 .model-count{background:rgba(0,0,0,0.05);color:#666;font-size:11px;padding:1px 6px;border-radius:4px;}
 .agent-grid{display:grid;/*手机端一行两列，充分利用宽度*/grid-template-columns:repeat(2,1fr);gap:8px;}
 .agent-mini{background:#fff;border:1px solid #eee;border-radius:6px;padding:8px;display:flex;flex-direction:column;justify-content:center;transition:background 0.2s;}
 .agent-mini-top{display:flex;align-items:center;margin-bottom:4px;}
 .agent-idx{background:#e6f0ff;color:#0066cc;font-size:10px;font-weight:700;min-width:18px;height:18px;border-radius:4px;display:flex;align-items:center;justify-content:center;margin-right:6px;flex-shrink:0;}
 .agent-mini-name{font-size:14px;font-weight:600;color:#333;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;}
 .agent-mini-desc{font-size:11px;color:#999;overflow:hidden;white-space:nowrap;text-overflow:ellipsis;}
 .provider-section { margin-bottom: 20px; break-inside: avoid; }
 .provider-title { font-size: 14px; font-weight: 700; color: #555; margin-bottom: 8px; padding-left: 4px; border-left: 3px solid #666; line-height: 1.2; }
 .chip-container { display: flex; flex-wrap: wrap; gap: 8px; }
 .chip { background: #fff; border: 1px solid #ddd; border-radius: 6px; padding: 6px 10px; display: flex; align-items: center; font-size: 13px; color: #333; box-shadow: 0 1px 2px rgba(0,0,0,0.02); }
 .chip-idx { background: #f0f0f0; color: #666; font-size: 11px; padding: 2px 5px; border-radius: 4px; margin-right: 6px; font-family: monospace; font-weight: 600; }
 .chip-name { font-weight: 500; }
 .chip-badge { margin-left: 6px; background: #e6f0ff; color: #0066cc; font-size: 10px; padding: 1px 5px; border-radius: 10px; font-weight: 600; }
 .mod-group { margin-bottom: 16px; break-inside: avoid; }
 .mod-title { font-size: 13px; font-weight: 700; color: #666; margin-bottom: 8px; text-transform: uppercase; letter-spacing: 0.5px; border-left: 3px solid #0066cc; padding-left: 6px; }
 .chip-box { display: flex; flex-wrap: wrap; gap: 8px; }
 .chip { background: #fff; border: 1px solid #e0e0e0; border-radius: 6px; padding: 6px 10px; display: flex; align-items: center; font-size: 13px; color: #333; transition: all 0.2s; }
 .chip-idx { background: #f5f5f5; color: #888; font-size: 11px; padding: 2px 6px; border-radius: 4px; margin-right: 8px; font-family: monospace; font-weight: 600; }
 .chip-name { font-weight: 500; }
 .chip-bad { margin-left: 8px; background: #e6f7ff; color: #1890ff; font-size: 10px; padding: 2px 6px; border-radius: 10px; font-weight: 600; } "#;
    let html = format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><style>{css}</style></head><body><div class="md"><div class="title">{title}</div>{html_body}</div></body></html>"#
    );

    let browser = Browser::instance().await;
    let tab = browser.new_tab().await?;

    let width = 600;
    tab.set_viewport(&Viewport::new(width, 100).with_device_scale_factor(2.0))
        .await?;

    tab.set_content(&html).await?;

    time::sleep(Duration::from_millis(200)).await;

    let height_js = "document.body.scrollHeight";
    let body_height = tab.evaluate(height_js).await?.as_f64().unwrap_or(800.0) as u32;

    let viewport = Viewport::new(width, body_height + 100).with_device_scale_factor(2.0);
    tab.set_viewport(&viewport).await?;

    time::sleep(Duration::from_millis(100)).await;

    let opts = CaptureOptions::new()
        .with_viewport(viewport)
        .with_quality(90);

    let b64 = tab
        .find_element(".md")
        .await?
        .screenshot_with_options(opts)
        .await?;

    let _ = tab.close().await;
    Ok(b64)
}

pub async fn get_full_content(
    ctx: &Context,
    writer: &LockedWriter,
    trigger_name: Option<&str>,
) -> (String, Vec<String>) {
    use simd_json::derived::{
        ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar,
    };

    let mut quote_text = String::new();
    let mut imgs = Vec::new();

    let onebot_event = match &ctx.event {
        crate::event::EventType::Onebot(e) => e,
        _ => return (quote_text, imgs),
    };

    let message_arr = match onebot_event.get_array("message") {
        Some(arr) => arr,
        None => return (quote_text, imgs),
    };

    // 1. 处理引用消息
    if let Some(reply) = message_arr
        .iter()
        .find(|s| s.get_str("type") == Some("reply"))
        && let Some(data) = reply.get("data")
    {
        let id_str_opt: Option<String> = match data.get_str("id") {
            Some(s) => Some(s.to_string()),
            None => data.get_i64("id").map(|i| i.to_string()),
        };
        if let Some(id_str) = id_str_opt
            && let Ok(id) = id_str.parse::<i32>()
            && let Ok(ret) = api::get_msg(ctx, writer.clone(), id).await
        {
            let mut temp_text = String::new();
            // 这里 Message 结构体内部也是 Segment 列表
            for seg in &ret.message.0 {
                match seg.type_.as_str() {
                    "text" => {
                        if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
                            temp_text.push_str(t);
                        }
                    }
                    "image" => {
                        if let Some(u) = seg.data.get("url").and_then(|v| v.as_str()) {
                            imgs.push(u.to_string());
                        }
                    }
                    "video" => {
                        let url = seg
                            .data
                            .get("url")
                            .or(seg.data.get("file"))
                            .and_then(|v| v.as_str());
                        if let Some(u) = url {
                            imgs.push(u.to_string());
                        }
                    }
                    _ => {}
                }
            }

            let trimmed = temp_text.trim();
            if !trimmed.is_empty() {
                for line in trimmed.lines() {
                    quote_text.push_str("> ");
                    quote_text.push_str(line);
                    quote_text.push('\n');
                }
                quote_text.push('\n');
            }
        }
    }

    // 2. 提取当前消息内容
    let mut found_trigger = false;

    for seg in message_arr {
        let type_ = seg.get_str("type").unwrap_or("");
        let data = seg.get("data");

        if type_ == "image" {
            if let Some(u) = data.and_then(|d| d.get_str("url")) {
                imgs.push(u.to_string());
            }
        } else if type_ == "video" {
            if let Some(d) = data {
                let url = d.get_str("url").or(d.get_str("file"));
                if let Some(u) = url {
                    imgs.push(u.to_string());
                }
            }
        } else if type_ == "text" {
            if let Some(name) = trigger_name
                && !found_trigger
            {
                let text = data.and_then(|d| d.get_str("text")).unwrap_or("");
                let norm_text = normalize(text).to_lowercase();
                let norm_name = normalize(name).to_lowercase();
                if norm_text.contains(&norm_name) {
                    found_trigger = true;
                }
            }
        } else if type_ == "at"
            && found_trigger
            && let Some(d) = data
        {
            let qq = d
                .get_str("qq")
                .map(|s| s.to_string())
                .or_else(|| d.get_i64("qq").map(|i| i.to_string()))
                .or_else(|| d.get_u64("qq").map(|i| i.to_string()));

            if let Some(id) = qq
                && id != "all"
            {
                imgs.push(format!("https://q.qlogo.cn/g?b=qq&nk={}&s=640", id));
            }
        }
    }

    (quote_text, imgs)
}

pub fn format_history(
    hist: &[super::types::ChatMessage],
    offset: usize,
    text_mode: bool,
) -> String {
    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();

    hist.iter()
        .enumerate()
        .map(|(i, m)| {
            let emoji = match m.role.as_str() {
                "user" => "👤",
                "assistant" => "🤖",
                "system" => "⚙️",
                _ => "❓",
            };
            let time = chrono::DateTime::from_timestamp(m.timestamp, 0)
                .map(|dt| {
                    use chrono::TimeZone;
                    chrono::Local
                        .from_utc_datetime(&dt.naive_utc())
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();

            let mut body = m.content.clone();
            if text_mode {
                body = re.replace_all(&body, "[图片]").to_string();
            }

            if !m.images.is_empty() {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                if text_mode {
                    let links = m
                        .images
                        .iter()
                        .map(|u| {
                            if u.starts_with("data:") {
                                "- [Base64 Image]".to_string()
                            } else {
                                format!("- [图片] {}", u)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    body.push_str(&links);
                } else {
                    let imgs = m
                        .images
                        .iter()
                        .map(|u| format!("![image]({})", u))
                        .collect::<Vec<_>>()
                        .join("\n");
                    body.push_str(&imgs);
                }
            }

            if body.trim().is_empty() {
                body = "(无内容)".to_string();
            }
            format!("**#{} {} {}**\n{}", offset + i + 1, emoji, time, body)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

pub fn truncate_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[..max_chars].iter().collect::<String>() + "..."
    }
}

pub fn format_export_txt(
    agent_name: &str,
    model: &str,
    scope: &str,
    hist: &[super::types::ChatMessage],
) -> String {
    let re = Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();
    let mut content = String::new();
    let separator = "─".repeat(40);
    let thin_sep = "┄".repeat(40);

    content.push_str(&format!("┏{}┓\n", "━".repeat(40)));
    content.push_str(&format!("┃  智能体: {:<32}┃\n", agent_name));
    content.push_str(&format!("┃  模  型: {:<32}┃\n", model));
    content.push_str(&format!("┃  类  型: {:<32}┃\n", scope));
    content.push_str(&format!(
        "┃  导  出: {:<32}┃\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    content.push_str(&format!("┃  记录数: {:<32}┃\n", hist.len()));
    content.push_str(&format!("┗{}┛\n\n", "━".repeat(40)));

    for (i, m) in hist.iter().enumerate() {
        let time = chrono::DateTime::from_timestamp(m.timestamp, 0)
            .map(|t| {
                use chrono::TimeZone;
                chrono::Local
                    .from_utc_datetime(&t.naive_utc())
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "未知时间".to_string());

        let role_name = match m.role.as_str() {
            "user" => "👤 用户",
            "assistant" => "🤖 助手",
            "system" => "⚙️ 系统",
            _ => &m.role,
        };

        content.push_str(&format!("【#{} {} | {}】\n", i + 1, role_name, time));
        content.push_str(&format!("{}\n", thin_sep));

        let clean_content = re.replace_all(&m.content, "[图片数据]");
        content.push_str(&clean_content);
        content.push('\n');

        if !m.images.is_empty() {
            content.push_str(&format!("\n📷 附图 ({} 张):\n", m.images.len()));
            for (j, url) in m.images.iter().enumerate() {
                if url.starts_with("data:") {
                    content.push_str(&format!("   {}. [Base64 Image Data]\n", j + 1));
                } else {
                    content.push_str(&format!("   {}. {}\n", j + 1, url));
                }
            }
        }
        content.push_str(&format!("\n{}\n\n", separator));
    }
    content
}
