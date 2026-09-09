//! OpenAI 兼容的图像生成接口（`POST /v1/images/generations`）。
//!
//! 站点上的 `gpt-image-2.5-*` 系列走专用图像接口，尺寸、画质这类绘图参数由接口
//! 直接接收；聊天补全虽然偶尔也能吐出 markdown 图片，但拿不到这些参数，遇到
//! 多图/编辑等场景也不如专用接口稳。这里把提示词交给图像接口，再把结果拼回与
//! 聊天补全一致的 markdown 图片链接，复用下游的提取、发送与历史记录逻辑。

use super::logic::Reply;
use super::types::{Agent, ChatMessage};
use anyhow::{Context as _, anyhow};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// 默认走图像接口的模型关键字（不区分大小写、子串匹配）。
/// 站点上新图像模型时改 `[oai] image_models` 即可，不必改代码。
pub(super) const DEFAULT_IMAGE_MODELS: &[&str] = &["gpt-image-2.5"];

/// 单次绘图请求的上限。真正的总预算由 `[oai] request_timeout_seconds` 兜底，
/// 这里略小一些，好让超时错误落在「图像接口」而不是笼统的「请求超时」。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(240);
/// 图像接口一次最多返回 1 张（站点限制），保留 1 张以防将来放开。
const MAX_IMAGES: usize = 1;

#[derive(Debug, Default, Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageItem>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Default, Deserialize)]
struct ImageItem {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    revised_prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

/// 模型是否走 `/v1/images/generations`。`keywords` 为空时视为不启用。
pub(super) fn is_images_model(model: &str, keywords: &[String]) -> bool {
    let lower = model.trim().to_lowercase();
    keywords
        .iter()
        .filter(|keyword| !keyword.trim().is_empty())
        .any(|keyword| lower.contains(&keyword.trim().to_lowercase()))
}

/// 绘图参数：提示词去掉参数后剩下的部分，以及可选的尺寸与画质。
#[derive(Debug, Default, PartialEq)]
struct Options {
    prompt: String,
    size: Option<String>,
    quality: Option<String>,
}

/// 从提示词里剥离 `--size 1536x1024` / `-s`、`--quality high` / `-q`。
/// 参数缺值或取值非法时原样留在提示词里，避免把用户想画的东西悄悄吃掉。
fn parse_options(input: &str) -> Options {
    let mut words: Vec<&str> = Vec::new();
    let mut size = None;
    let mut quality = None;
    let mut tokens = input.split_whitespace().peekable();

    while let Some(token) = tokens.next() {
        match token {
            "--size" | "-s" | "尺寸" => match tokens.peek().filter(|value| is_size(value)) {
                Some(value) => {
                    size = Some((*value).to_lowercase());
                    tokens.next();
                }
                None => words.push(token),
            },
            "--quality" | "-q" | "画质" => {
                match tokens.peek().filter(|value| is_quality(value)) {
                    Some(value) => {
                        quality = Some((*value).to_lowercase());
                        tokens.next();
                    }
                    None => words.push(token),
                }
            }
            _ => words.push(token),
        }
    }

    Options {
        prompt: words.join(" "),
        size,
        quality,
    }
}

fn is_size(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    if value == "auto" {
        return true;
    }
    match value.split_once('x') {
        Some((width, height)) => {
            !width.is_empty()
                && !height.is_empty()
                && width.chars().all(|c| c.is_ascii_digit())
                && height.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_quality(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "auto" | "low" | "medium" | "high"
    )
}

/// 最后一条用户消息就是本轮的绘图提示词；房间系统提示词作为风格前缀。
fn last_user_prompt(hist: &[ChatMessage]) -> String {
    hist.iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.trim().to_string())
        .unwrap_or_default()
}

/// 调用图像接口，把结果包装成与聊天补全一致的 `Reply`（正文含 markdown 图片链接）。
pub(super) async fn generate_reply(
    api_base: &str,
    api_key: &str,
    agent: &Agent,
    hist: &[ChatMessage],
) -> anyhow::Result<Reply> {
    let options = parse_options(&last_user_prompt(hist));
    let prompt = match (agent.system_prompt.trim(), options.prompt.trim()) {
        ("", user) => user.to_string(),
        (system, "") => system.to_string(),
        (system, user) => format!("{system}\n{user}"),
    };
    if prompt.trim().is_empty() {
        return Err(anyhow!("请输入绘图提示词，例如：画图 一只在窗台晒太阳的橘猫"));
    }

    let mut body = json!({
        "model": agent.model,
        "prompt": prompt,
        "n": 1,
        // URL 比 base64 短得多：base64 会被整段写进历史，撑大配置文件。
        "response_format": "url",
    });
    if let Some(size) = &options.size {
        body["size"] = json!(size);
    }
    if let Some(quality) = &options.quality {
        body["quality"] = json!(quality);
    }

    let endpoint = format!("{}/images/generations", api_base.trim_end_matches('/'));
    let response = crate::http::client()
        .post(&endpoint)
        .bearer_auth(api_key)
        .json(&body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("请求 {endpoint} 失败"))?;

    let status = response.status();
    let bytes = response.bytes().await.context("读取图像响应失败")?;
    let ImagesResponse {
        data,
        model,
        error,
    } = serde_json::from_slice(&bytes).unwrap_or_default();

    if !status.is_success() {
        let detail = error
            .map(|error| error.message)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| excerpt(&bytes));
        return Err(anyhow!("图像接口返回 HTTP {}：{}", status.as_u16(), detail));
    }

    let mut links = Vec::new();
    let mut revised = None;
    for item in data.into_iter().take(MAX_IMAGES) {
        if let Some(url) = item.url.filter(|url| !url.trim().is_empty()) {
            links.push(format!("![image]({})", url.trim()));
        } else if let Some(b64) = item.b64_json.filter(|b64| !b64.trim().is_empty()) {
            links.push(format!("![image](data:image/png;base64,{})", b64.trim()));
        }
        if revised.is_none() {
            revised = item
                .revised_prompt
                .filter(|prompt| !prompt.trim().is_empty());
        }
    }
    if links.is_empty() {
        let detail = error
            .map(|error| error.message)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| "接口未返回任何图片".to_string());
        return Err(anyhow!("图像接口未返回图片：{detail}"));
    }

    let caption = revised
        .or_else(|| {
            (!options.prompt.trim().is_empty()).then(|| options.prompt.trim().to_string())
        })
        .unwrap_or_else(|| "绘图完成".to_string());
    // 提示词里带换行（系统风格前缀 + 用户输入），压成一行后加粗标题才不会被拆开。
    let caption = super::utils::truncate_str(&one_line(&caption), 160);

    Ok(Reply {
        text: format!("🎨 **{}**\n\n{}", caption, links.join("\n")),
        sources: Vec::new(),
        trace: Vec::new(),
        trace_overflow: 0,
        model: Some(model.unwrap_or_else(|| agent.model.clone())),
    })
}

fn excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned()
}

/// 折叠空白成单行，用于把多行提示词放进加粗标题。
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_configured_keywords_case_insensitively() {
        let keywords = vec!["gpt-image-2.5".to_string()];
        assert!(is_images_model("gpt-image-2.5-flare", &keywords));
        assert!(is_images_model("GPT-IMAGE-2.5-SUNBURST", &keywords));
        assert!(!is_images_model("gpt-image-2", &keywords));
        assert!(!is_images_model("gpt-5.6-luna", &keywords));
        assert!(!is_images_model("gpt-image-2.5-flare", &[]));
    }

    #[test]
    fn parses_and_strips_drawing_flags() {
        let options = parse_options("一只猫 --size 1536x1024 -q high");
        assert_eq!(options.prompt, "一只猫");
        assert_eq!(options.size.as_deref(), Some("1536x1024"));
        assert_eq!(options.quality.as_deref(), Some("high"));

        let options = parse_options("一只猫 -s auto 画质 medium");
        assert_eq!(options.prompt, "一只猫");
        assert_eq!(options.size.as_deref(), Some("auto"));
        assert_eq!(options.quality.as_deref(), Some("medium"));
    }

    #[test]
    fn keeps_invalid_or_valueless_flags_in_the_prompt() {
        let options = parse_options("一只猫 -s 很大 --quality");
        assert_eq!(options.prompt, "一只猫 -s 很大 --quality");
        assert!(options.size.is_none());
        assert!(options.quality.is_none());
    }

    #[test]
    fn recognizes_size_and_quality_values() {
        assert!(is_size("1024x1024"));
        assert!(is_size("AUTO"));
        assert!(!is_size("1024"));
        assert!(!is_size("1024X"));
        assert!(is_quality("High"));
        assert!(!is_quality("ultra"));
    }

    #[test]
    fn collapses_multiline_captions() {
        assert_eq!(one_line("写实摄影\n一只猫"), "写实摄影 一只猫");
        assert_eq!(one_line("  a   b\tc "), "a b c");
    }
}
