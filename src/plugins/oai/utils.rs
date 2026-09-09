use crate::adapters::satori::{LockedWriter, api, forward};
use crate::event::Context;
use regex::Regex;
use simd_json::base::ValueAsScalar;
use std::sync::OnceLock;

pub static RE_API: OnceLock<Regex> = OnceLock::new();
pub static RE_IDX: OnceLock<Regex> = OnceLock::new();

/// 默认保留的模型关键字：中转站一次吐出近千个 id，其中绝大多数是历史快照、
/// 小参数量档位和语音/视频等与聊天无关的条目。这里只列当前仍值得用的旗舰对话
/// 与图像模型；关键字按不区分大小写的子串匹配，因此写到"系列"粒度即可。
///
/// 站点上新时改 `[oai] model_filter.keep` 即可，不必改代码。
pub const DEFAULT_MODEL_KEEP: &[&str] = &[
    // OpenAI
    "gpt-5.6",
    "gpt-5.5",
    "gpt-image-2.5",
    // Anthropic
    "claude-opus-5",
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    // Google
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.1-pro-preview",
    "gemini-3-pro-image",
    "gemini-3.1-flash-image",
    // xAI
    "grok-4.6",
    "grok-4.5",
    // 国内
    "deepseek-v4-pro",
    "deepseek-v4-flash",
    "kimi-k3",
    "kimi-k2.6",
    "qwen3.8-max",
    "glm-5.3",
    "minimax-m2.7",
    "mimo-v2.5",
    "doubao-seedream-5-0",
];

/// 默认剔除的模型关键字，优先于 `DEFAULT_MODEL_KEEP`。
/// 命中的多是同一模型的重复投影：带发布日期的快照、低算力/低分辨率档位，
/// 以及中转站自己加的 `-all` / `thinking-*` 之类的聚合别名。
pub const DEFAULT_MODEL_DROP: &[&str] = &[
    "-2025-",
    "-2026-",
    "-thinking-low",
    "-thinking-minimal",
    "thinking-*",
    "-lite",
    "-512px",
    "-2k",
    "-4k",
    "-all",
    "-beta",
    "customtools",
    "highspeed",
    "lightning",
    "-image-preview",
    "-vision-exp",
];

/// 模型列表过滤规则。两份关键字都不区分大小写、按子串匹配。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ModelFilterConfig {
    /// 保留：只留下命中任一关键字的模型。留空表示不筛选，接受站点返回的全部模型。
    pub keep: Vec<String>,
    /// 剔除：命中任一关键字的模型一律去掉，优先于 `keep`。
    pub drop: Vec<String>,
}

impl Default for ModelFilterConfig {
    fn default() -> Self {
        Self {
            keep: DEFAULT_MODEL_KEEP.iter().map(|s| s.to_string()).collect(),
            drop: DEFAULT_MODEL_DROP.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl ModelFilterConfig {
    /// 单个模型 id 是否留下。
    pub fn accepts(&self, model: &str) -> bool {
        let lower = model.to_lowercase();
        let hit = |list: &[String]| {
            list.iter()
                .filter(|kw| !kw.trim().is_empty())
                .any(|kw| lower.contains(&kw.to_lowercase()))
        };
        if hit(&self.drop) {
            return false;
        }
        self.keep.iter().all(|kw| kw.trim().is_empty()) || hit(&self.keep)
    }
}

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

pub fn filter_models(models: &[String], filter: &ModelFilterConfig) -> Vec<String> {
    models
        .iter()
        .filter(|m| filter.accepts(m))
        .cloned()
        .collect()
}

/// 模型 id 归属的厂商分组名，用于模型列表分区展示。
///
/// 分组不跟着过滤关键字走：关键字可由管理员随意增删，拿它当标题会拆出
/// 「Gpt-5.6 Series」这类碎片；厂商前缀稳定得多，也更接近用户的心智。
pub fn model_vendor(model: &str) -> &'static str {
    let lower = model.to_lowercase();
    const VENDORS: &[(&str, &str)] = &[
        ("gpt-", "OpenAI"),
        ("o1", "OpenAI"),
        ("o3", "OpenAI"),
        ("o4", "OpenAI"),
        ("chatgpt", "OpenAI"),
        ("claude", "Anthropic"),
        ("gemini", "Google"),
        ("grok", "xAI"),
        ("deepseek", "DeepSeek"),
        ("kimi", "Moonshot"),
        ("moonshot", "Moonshot"),
        ("qwen", "Qwen"),
        ("qwq", "Qwen"),
        ("glm", "智谱 GLM"),
        ("minimax", "MiniMax"),
        ("mimo", "MiMo"),
        ("doubao", "豆包"),
        ("hunyuan", "混元"),
        ("mj", "Midjourney"),
    ];
    VENDORS
        .iter()
        .find(|(prefix, _)| lower.starts_with(prefix))
        .map(|(_, name)| *name)
        .unwrap_or("其他")
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

/// 一条转发不该把视觉预算吃光。
const MAX_FORWARD_IMAGES: usize = 4;

/// 展开一条合并转发，按引用块并进提示词，顺带把转发里的图片交给视觉输入。
async fn append_forward(
    ctx: &Context,
    writer: &LockedWriter,
    source: forward::Source,
    label: &str,
    quote_text: &mut String,
    imgs: &mut Vec<String>,
) {
    let source = match api::channel_id(ctx) {
        Ok(channel) => source.in_channel(channel),
        Err(_) => source,
    };
    let view = forward::expand(ctx, writer, source).await;
    let body = view.transcript();
    if body.trim().is_empty() {
        return;
    }
    quote_text.push_str(label);
    quote_text.push('\n');
    for line in body.lines() {
        quote_text.push_str("> ");
        quote_text.push_str(line);
        quote_text.push('\n');
    }
    quote_text.push('\n');
    for url in view.images().into_iter().take(MAX_FORWARD_IMAGES) {
        if !imgs.contains(&url) {
            imgs.push(url);
        }
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

            // 引用一条合并转发时，正文本身是空的：不展开等于什么都没引用。
            if let Some(source) = forward::source_of(&ret.message, Some(ret.message_id)) {
                append_forward(ctx, writer, source, "引用的合并转发：", &mut quote_text, &mut imgs)
                    .await;
            }
        }
    }

    // 2. 当前消息自带的合并转发同样要展开，否则只剩一个占位符。
    if let Some(source) = message_arr
        .iter()
        .find(|seg| seg.get_str("type") == Some("forward"))
        .and_then(|seg| seg.get("data"))
        .and_then(|data| data.get_str("id"))
        .map(|id| forward::Source::new(Some(id.to_string()), Some(crate::event::MessageEvent(event).message_id())))
    {
        append_forward(
            ctx,
            writer,
            source,
            "本条消息里的合并转发：",
            &mut quote_text,
            &mut imgs,
        )
        .await;
    }

    // 3. 提取当前消息内容
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


pub(crate) fn format_elapsed(started: std::time::Instant) -> String {
    let seconds = started.elapsed().as_secs_f32();
    if seconds >= 60.0 {
        format!("{}分{:.0}秒", (seconds / 60.0) as u32, seconds % 60.0)
    } else {
        format!("{seconds:.1}秒")
    }
}

/// 超长文本的中间省略：两端各留一半，省略号落在中间。
///
/// 尾部截断会把 shell 命令的参数、URL 的路径这类关键信息整段吃掉，而它们恰恰
/// 是判断「这次工具调用做了什么」的依据。
pub(crate) fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars || max_chars < 8 {
        return value.to_string();
    }
    let keep = max_chars - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ModelFilterConfig, model_vendor, openai_api_base};

    #[test]
    fn middle_truncation_keeps_both_ends() {
        assert_eq!(super::truncate_middle("短", 8), "短");
        let long = "bash -lc 'echo 中间省略 && ls -la /data/data/com.termux/files/home'";
        let cut = super::truncate_middle(long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.starts_with("bash -lc"), "{cut}");
        assert!(cut.ends_with("home'"), "{cut}");
        assert!(cut.contains('…'), "{cut}");
    }

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

    #[test]
    fn default_filter_keeps_flagships_and_drops_noise() {
        let filter = ModelFilterConfig::default();
        for keep in [
            "gpt-5.6-luna",
            "claude-opus-5-thinking",
            "gemini-3.8-flash",
            "grok-4.6",
            "deepseek-v4-pro",
            "glm-5.3",
            "MiniMax-M2.7",
            "gpt-image-2.5-flare",
            "gpt-image-2.5-sunburst",
        ] {
            assert!(filter.accepts(keep), "{keep} 应当保留");
        }
        for drop in [
            "gpt-4o",
            "gpt-image-2",
            "gpt-image-2-all",
            "claude-3-5-sonnet-20241022",
            "gpt-5.5-2026-04-23",
            "gemini-3.1-flash-lite-preview",
            "gemini-3.8-flash-thinking-low",
            "gpt-5-thinking-all",
            "MiniMax-Hailuo-2.3",
            "minimax/speech-2.6-hd",
            "qwen3-embedding-8b",
        ] {
            assert!(!filter.accepts(drop), "{drop} 应当剔除");
        }
    }

    #[test]
    fn drop_wins_and_empty_keep_accepts_everything() {
        let filter = ModelFilterConfig {
            keep: vec!["gpt-5".into()],
            drop: vec!["-pro".into()],
        };
        assert!(filter.accepts("gpt-5.6"));
        assert!(!filter.accepts("gpt-5.6-pro"), "剔除优先于保留");
        assert!(!filter.accepts("claude-opus-5"));

        let passthrough = ModelFilterConfig {
            keep: vec![],
            drop: vec![],
        };
        assert!(passthrough.accepts("whatever-1"));

        // 关键字不区分大小写，管理员照抄站点上的原始 id 也能命中
        let cased = ModelFilterConfig {
            keep: vec!["MiniMax-M2".into()],
            drop: vec![],
        };
        assert!(cased.accepts("minimax-m2.7"));
    }

    #[test]
    fn vendor_grouping_covers_the_default_keeps() {
        assert_eq!(model_vendor("gpt-image-2.5-flare"), "OpenAI");
        assert_eq!(model_vendor("claude-fable-5-1"), "Anthropic");
        assert_eq!(model_vendor("gemini-3-pro-image"), "Google");
        assert_eq!(model_vendor("MiniMax-M2.7"), "MiniMax");
        assert_eq!(model_vendor("mj"), "Midjourney");
        assert_eq!(model_vendor("something-else"), "其他");
    }
}
