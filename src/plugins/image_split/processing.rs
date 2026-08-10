use base64::{Engine as _, engine::general_purpose};
use image::GenericImageView;
use std::io::Cursor;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub async fn download_image(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::get(url).await.map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

/// 阻塞执行图片裁剪，返回 Base64 列表
pub fn split_image_blocking(img_bytes: Vec<u8>, rows: u32, cols: u32) -> Result<Vec<String>> {
    let img = image::load_from_memory(&img_bytes)
        .map_err(|e| format!("Failed to load image from memory: {}", e))?;

    let (width, height) = img.dimensions();
    let tile_width = width / cols;
    let tile_height = height / rows;

    if tile_width == 0 || tile_height == 0 {
        return Err("图片太小，无法按照指定规格裁剪".into());
    }

    let mut base64_list = Vec::with_capacity((rows * cols) as usize);

    for r in 0..rows {
        for c in 0..cols {
            let x = c * tile_width;
            let y = r * tile_height;

            // crop_imm 是不可变裁剪，开销较小
            let sub_img = img.view(x, y, tile_width, tile_height).to_image();

            let mut buffer = Cursor::new(Vec::new());
            sub_img
                .write_to(&mut buffer, image::ImageFormat::Png)
                .map_err(|e| format!("Failed to encode sub-image: {}", e))?;

            let b64 = general_purpose::STANDARD.encode(buffer.get_ref());
            base64_list.push(b64);
        }
    }

    Ok(base64_list)
}
