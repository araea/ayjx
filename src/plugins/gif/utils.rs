use crate::adapters::onebot::{LockedWriter, api};
use crate::event::Context;
use regex::Regex;
use simd_json::OwnedValue;
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::OnceLock;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 提取消息中的图片 URL (支持直接发送、引用回复)
/// args: 指令后的参数列表 (可能包含图片)
/// reply_id: 引用回复的消息 ID
pub async fn get_image_url(
    ctx: &Context,
    writer: LockedWriter,
    args: &[OwnedValue],
    reply_id: Option<&String>,
) -> Option<String> {
    // 1. 检查指令参数中是否包含图片
    for seg in args {
        if seg.get_str("type") == Some("image")
            && let Some(data) = seg.get("data")
            && let Some(url) = data.get_str("url")
        {
            return Some(url.to_string());
        }
    }

    // 2. 检查引用消息
    if let Some(rid_str) = reply_id {
        // 尝试解析 ID
        let rid = rid_str
            .parse::<i32>()
            .ok()
            .or_else(|| rid_str.parse::<i64>().map(|v| v as i32).ok())?;

        // 调用 API 获取原消息
        if let Ok(resp) = api::get_msg(ctx, writer, rid).await {
            for seg in resp.message.0 {
                if seg.type_ == "image"
                    && let Some(url_val) = seg.data.get("url")
                    && let Some(url) = url_val.as_str()
                {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

/// 下载图片
pub async fn download_image(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::get(url).await.map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

/// 解析 "3x3" 或 "3*3" 或 "3×3" 等格式 (大小写不敏感)
pub fn parse_grid_dim(s: &str) -> Option<(u32, u32)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)(\d+)\s*[xX*×]\s*(\d+)").unwrap());
    re.captures(s).and_then(|caps| {
        let r = caps[1].parse().ok().filter(|&v| v > 0)?;
        let c = caps[2].parse().ok().filter(|&v| v > 0)?;
        Some((r, c))
    })
}

pub fn format_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.2} MB", bytes as f64 / MB)
    } else {
        format!("{:.2} KB", bytes as f64 / KB)
    }
}
