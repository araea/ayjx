use crate::adapters::satori::{LockedWriter, api};
use crate::event::Context;
use regex::Regex;
use simd_json::base::ValueAsScalar;
use std::sync::OnceLock;

pub static RE_API: OnceLock<Regex> = OnceLock::new();
pub static RE_IDX: OnceLock<Regex> = OnceLock::new();

pub const MODEL_KEYWORDS: &[&str] = &[
    "gpt-5.6",
    "gpt-5.5",
    "gpt-image",
    "claude-mythos-5",
    "claude-opus-5",
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-opus-4",
    "gemini-3.7",
    "gemini-3.6",
    "gemini-3.5",
    "gemini-3.1",
    "deepseek-v4",
    "kimi-k3",
    "qwen3.8",
    "qwen3.7",
    "grok-4",
    "glm-5",
    "minimax",
    "hy3",
    "mimo",
];

/// async-openai 会在 API 基址后拼接 `/chat/completions`。管理员只填写服务裸域名时，
/// 自动补齐 OpenAI 兼容接口通用的 `/v1`，已有自定义路径则原样保留。
pub fn openai_api_base(configured: &str) -> String {
    let configured = configured.trim().trim_end_matches('/');
    let Ok(mut parsed) = url::Url::parse(configured) else {
        return configured.to_string();
    };
    if parsed.path().is_empty() || parsed.path() == "/" {
        parsed.set_path("/v1");
    }
    parsed.to_string().trim_end_matches('/').to_string()
}

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
            '０' => '0',
            '１' => '1',
            '２' => '2',
            '３' => '3',
            '４' => '4',
            '５' => '5',
            '６' => '6',
            '７' => '7',
            '８' => '8',
            '９' => '9',
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

    let event = match &ctx.event {
        crate::event::EventType::Satori(e) => e,
        _ => return (quote_text, imgs),
    };

    let message_arr = match event.get_array("message") {
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
            && let Ok(id) = id_str.parse::<i64>()
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
                        if let Some(u) = seg
                            .data
                            .get("url")
                            .or_else(|| seg.data.get("file"))
                            .and_then(|v| v.as_str())
                        {
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
            if let Some(u) = data.and_then(|d| d.get_str("url").or_else(|| d.get_str("file"))) {
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

#[cfg(test)]
mod tests {
    use super::openai_api_base;

    #[test]
    fn adds_v1_only_to_bare_openai_hosts() {
        assert_eq!(
            openai_api_base("https://api.apilio.ai"),
            "https://api.apilio.ai/v1"
        );
        assert_eq!(
            openai_api_base("https://example.com/v1/"),
            "https://example.com/v1"
        );
        assert_eq!(
            openai_api_base("https://example.com/openai"),
            "https://example.com/openai"
        );
    }
}
