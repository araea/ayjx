use base64::{Engine as _, engine::general_purpose};
use simd_json::OwnedValue;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::io::{Cursor, Read, Seek, SeekFrom};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn parse_png(bytes: &[u8]) -> Result<(String, String)> {
    let mut cursor = Cursor::new(bytes);
    let mut header = [0u8; 8];
    cursor.read_exact(&mut header)?;
    if header != [137, 80, 78, 71, 13, 10, 26, 10] {
        return Err("不是有效的 PNG 图片".into());
    }

    let mut ccv3_data: Option<String> = None;
    let mut chara_data: Option<String> = None;

    loop {
        let mut len_buf = [0u8; 4];
        if cursor.read_exact(&mut len_buf).is_err() {
            break;
        }
        let length = u32::from_be_bytes(len_buf) as u64;

        let mut type_buf = [0u8; 4];
        cursor.read_exact(&mut type_buf)?;
        let chunk_type = std::str::from_utf8(&type_buf).unwrap_or("");

        if chunk_type == "tEXt" {
            let mut data_buf = vec![0u8; length as usize];
            cursor.read_exact(&mut data_buf)?;
            if let Some(null_pos) = data_buf.iter().position(|&b| b == 0)
                && let Ok(keyword) = std::str::from_utf8(&data_buf[..null_pos])
            {
                let text_bytes = &data_buf[null_pos + 1..];
                if let Ok(text) = std::str::from_utf8(text_bytes) {
                    let key_lower = keyword.to_lowercase();
                    if key_lower == "ccv3" {
                        ccv3_data = Some(text.to_string());
                    } else if key_lower == "chara" {
                        chara_data = Some(text.to_string());
                    }
                }
            }
            cursor.seek(SeekFrom::Current(4))?; // Skip CRC
        } else {
            cursor.seek(SeekFrom::Current((length + 4) as i64))?;
        }
    }

    if let Some(b64) = ccv3_data.or(chara_data) {
        let mut json_str = decode_base64(&b64)?;

        // 解析为通用 JSON Value，不依赖特定 Struct
        let val: OwnedValue = unsafe { simd_json::serde::from_str(&mut json_str) }
            .map_err(|e| format!("JSON 解析失败: {}", e))?;

        // 尝试获取名字用于文件名 (优先 V3 的 data.name, 其次 V2 的 name)
        let name = if let Some(data) = val.get("data") {
            data.get_str("name").unwrap_or("character").to_string()
        } else {
            val.get_str("name").unwrap_or("character").to_string()
        };

        // 格式化为美观的 JSON 字符串
        let full_json = simd_json::to_string_pretty(&val)?;
        return Ok((name, full_json));
    }

    Err("未找到角色卡数据 (chara/ccv3)".into())
}

fn decode_base64(input: &str) -> Result<String> {
    let bytes = general_purpose::STANDARD.decode(input)?;
    Ok(String::from_utf8(bytes)?)
}
