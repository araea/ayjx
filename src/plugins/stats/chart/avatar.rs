use super::data_loader::BarData;
use super::utils::{create_default_avatar, get_average_color, make_circular_avatar};
use crate::plugins::get_data_dir;
use image::RgbaImage;
use image::imageops::FilterType;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::fs;

const AVATAR_SIZE: u32 = 100;
const CACHE_EXPIRE_DAYS: u64 = 3;

/// 批量处理头像下载与主题色提取
pub async fn prepare_avatars(data: &mut [BarData]) {
    let default_avatar = create_default_avatar(AVATAR_SIZE);

    // 获取缓存目录
    let cache_dir = match get_data_dir("stats").await {
        Ok(dir) => {
            let avatar_dir = dir.join("avatars");
            if !avatar_dir.exists() {
                let _ = fs::create_dir_all(&avatar_dir).await;
            }
            Some(avatar_dir)
        }
        Err(e) => {
            warn!(target: "Plugin/Stats", "无法获取数据目录: {}", e);
            None
        }
    };

    let futures: Vec<_> = data
        .iter_mut()
        .map(|item| {
            let url = item.avatar_url.clone();
            let cache_dir = cache_dir.clone();
            // 简单用 URL 的 hash 或 userID 做文件名，这里如果有 ID 优先用 ID
            let file_key = if let Some(uid) = item.user_id {
                format!("u_{}", uid)
            } else if let Some(u) = &url {
                format!("h_{:x}", md5::compute(u.as_bytes()))
            } else {
                "unknown".to_string()
            };

            async move {
                if let Some(url) = url {
                    download_avatar_cached(&url, &file_key, cache_dir, AVATAR_SIZE).await
                } else {
                    None
                }
            }
        })
        .collect();

    let avatar_results = futures_util::future::join_all(futures).await;

    for (i, avatar) in avatar_results.into_iter().enumerate() {
        if let Some(img) = avatar {
            data[i].theme_color = get_average_color(&img);
            data[i].avatar_img = Some(img);
        } else {
            data[i].avatar_img = Some(default_avatar.clone());
        }
    }
}

/// 下载头像并带文件缓存
async fn download_avatar_cached(
    url: &str,
    file_key: &str,
    cache_dir: Option<PathBuf>,
    size: u32,
) -> Option<RgbaImage> {
    let file_path = cache_dir.map(|dir| dir.join(format!("{}_{}.png", file_key, size)));

    // 1. 尝试从缓存读取
    if let Some(path) = &file_path
        && path.exists() {
            let should_refresh = if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(modified) = metadata.modified() {
                    match SystemTime::now().duration_since(modified) {
                        Ok(duration) => duration > Duration::from_secs(CACHE_EXPIRE_DAYS * 86400),
                        Err(_) => true,
                    }
                } else {
                    true
                }
            } else {
                true
            };

            if !should_refresh
                && let Ok(img) = image::open(path) {
                    return Some(img.to_rgba8());
                }
        }

    // 2. 下载
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;

    if let Ok(resp) = client.get(url).send().await
        && let Ok(bytes) = resp.bytes().await
        && let Ok(img) = image::load_from_memory(&bytes)
    {
        let resized = img.resize_exact(size, size, FilterType::Lanczos3);
        let circular = make_circular_avatar(&resized, size);

        // 3. 写入缓存
        if let Some(path) = &file_path {
            let png_data = encode_png(&circular);
            if let Some(data) = png_data {
                let _ = fs::write(path, data).await;
            }
        }

        return Some(circular);
    }

    // 如果下载失败但有旧缓存，勉强使用旧缓存
    if let Some(path) = &file_path
        && path.exists()
            && let Ok(img) = image::open(path) {
                return Some(img.to_rgba8());
            }

    None
}

fn encode_png(img: &RgbaImage) -> Option<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    img.write_to(&mut cursor, image::ImageFormat::Png).ok()?;
    Some(cursor.into_inner())
}
