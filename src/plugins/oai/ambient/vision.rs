//! 群聊图片 → 模型能收下的图片。
//!
//! 群里最常见的图恰恰是模型最常拒收的：QQ 表情包多是 GIF，而 Gemini 直接以
//! 400/500 回绝 `image/gif`——一张表情包就能让整轮判定失败。这里在送进模型之前
//! 统一过一道：认识的格式原样放行，不认识但解得开的（GIF 等）取首帧转成 PNG，
//! 解不开的丢掉。顺带把过大的图缩到边长上限，省 token 也省手机的 CPU。

use super::window::Turn;
use base64::Engine as _;
use std::io::Cursor;

/// 模型直接接受的图片类型。
const PASSTHROUGH: [&str; 5] = [
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/heic",
    "image/heif",
];

/// 转码前的体积上限；再大的图在手机上解码不划算。
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// 转码后的边长上限。
const MAX_EDGE: u32 = 1024;

/// 转码结果的缓存条数上限。一轮最多看两三张图，够覆盖判定与发言两次读取，
/// 也够覆盖同一批消息被连续几轮反复带进上下文。
const CACHE_ENTRIES: usize = 12;
/// 缓存活多久。图片本身不会变，但 QQ 的直链是签名的，攒着过期的条目没意义。
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// 直链 → （转好的 data URL 或「这张用不了」，记下的时刻）。
type Cache = std::sync::Mutex<
    std::collections::HashMap<String, (Option<String>, std::time::Instant)>,
>;

/// 一轮里同一张图至少要被取两次：判定看一次，人格发言再看一次。手机上这意味着
/// 两次下载加两次解码，白等好几秒——而等待时间是从「模拟打字」的预算里扣的，
/// 直接影响它看起来像不像在正常聊天。所以按直链缓存转码结果。
fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 缓存里那份（`Some(None)` 表示这张图确认过不能用，不必再下一遍）。
fn cached(url: &str) -> Option<Option<String>> {
    let mut guard = cache().lock().unwrap_or_else(|error| error.into_inner());
    let (value, at) = guard.get(url)?;
    if at.elapsed() > CACHE_TTL {
        guard.remove(url);
        return None;
    }
    Some(value.clone())
}

fn remember(url: &str, value: Option<String>) {
    let mut guard = cache().lock().unwrap_or_else(|error| error.into_inner());
    if guard.len() >= CACHE_ENTRIES {
        // 逐出最旧的一条；条数这么小，扫一遍比维护一个链表更省事也更好读。
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(url.to_string(), (value, std::time::Instant::now()));
}

/// 最近的若干张图片，转成可直接送进模型的 data URL，按时间正序。
///
/// 下载与转码都可能失败，失败的那张直接跳过——判定宁可少看一张图，也不该因为
/// 一张表情包整轮报废。
pub(crate) async fn usable_images(turns: &[Turn], limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for url in turns.iter().rev().flat_map(|turn| turn.images.iter().rev()) {
        let usable = match cached(url) {
            Some(hit) => hit,
            None => {
                let data_url = super::super::logic::to_data_url(url).await;
                let usable = normalize(&data_url);
                remember(url, usable.clone());
                usable
            }
        };
        if let Some(usable) = usable {
            out.push(usable);
            if out.len() >= limit {
                break;
            }
        }
    }
    out.reverse();
    out
}

/// data URL → 模型可接受的 data URL；无法使用时返回 `None`。
fn normalize(data_url: &str) -> Option<String> {
    let (header, payload) = data_url.split_once(',')?;
    let mime = header
        .strip_prefix("data:")?
        .strip_suffix(";base64")?
        .to_ascii_lowercase();
    if PASSTHROUGH.contains(&mime.as_str()) {
        return Some(data_url.to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let png = to_png(&bytes)?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    ))
}

/// 任意图片字节 → PNG（动图取首帧）。
fn to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    let mut image = image::load_from_memory(bytes).ok()?;
    if image.width() > MAX_EDGE || image.height() > MAX_EDGE {
        image = image.thumbnail(MAX_EDGE, MAX_EDGE);
    }
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    Some(png.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_url(mime: &str, bytes: &[u8]) -> String {
        format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    fn gif(width: u32, height: u32) -> Vec<u8> {
        let frame = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 9, 9, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(frame)
            .write_to(&mut out, image::ImageFormat::Gif)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn supported_types_pass_through_untouched() {
        let url = data_url("image/jpeg", b"not really a jpeg");
        assert_eq!(normalize(&url).as_deref(), Some(url.as_str()));
        assert!(normalize(&data_url("IMAGE/PNG", b"x")).is_some());
    }

    #[test]
    fn gifs_become_pngs_instead_of_failing_the_whole_turn() {
        let converted = normalize(&data_url("image/gif", &gif(8, 8))).expect("GIF 应当被转成 PNG");
        assert!(converted.starts_with("data:image/png;base64,"));
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(converted.split_once(',').unwrap().1)
            .unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Png
        );
    }

    #[test]
    fn oversized_frames_are_scaled_down() {
        let png = to_png(&gif(MAX_EDGE + 400, 40)).unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert!(image.width() <= MAX_EDGE, "{}", image.width());
    }

    #[test]
    fn the_cache_answers_twice_and_evicts_the_oldest() {
        let url = |n: usize| format!("https://example.com/{n}.png");
        for index in 0..CACHE_ENTRIES + 3 {
            remember(&url(index), Some(format!("data:{index}")));
        }
        assert!(cache().lock().unwrap().len() <= CACHE_ENTRIES);
        // 最新写进去的仍在，最早那批已经被逐出。
        assert_eq!(
            cached(&url(CACHE_ENTRIES + 2)),
            Some(Some(format!("data:{}", CACHE_ENTRIES + 2)))
        );
        assert_eq!(cached(&url(0)), None);
        // 确认过不能用的图也记下来，免得每轮重下一遍那张坏图。
        remember("https://example.com/broken.gif", None);
        assert_eq!(cached("https://example.com/broken.gif"), Some(None));
    }

    #[test]
    fn undecodable_payloads_are_dropped_rather_than_sent() {
        assert!(normalize(&data_url("image/gif", b"broken")).is_none());
        assert!(normalize("https://example.com/a.png").is_none());
        assert!(normalize(&data_url("application/pdf", b"%PDF-")).is_none());
    }
}
