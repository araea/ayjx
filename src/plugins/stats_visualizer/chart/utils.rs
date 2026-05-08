use crate::plugins::stats_visualizer::StatsConfig;
use base64::{Engine as _, engine::general_purpose};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use plotters::prelude::*;
use std::sync::OnceLock;

// ================= 字体查找 =================

static FONT_DB: OnceLock<fontdb::Database> = OnceLock::new();
static RESOLVED_FAMILY: OnceLock<String> = OnceLock::new();

fn get_font_db() -> &'static fontdb::Database {
    FONT_DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

fn is_font_available(family: &str) -> bool {
    let db = get_font_db();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..Default::default()
    };
    db.query(&query).is_some()
}

const CJK_FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK TC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
    "SimHei",
    "Microsoft YaHei",
];

/// Resolve the best available CJK font family name via fontdb.
/// Result is cached once — subsequent calls return the same family
/// without re-scanning.  fontdb scans font directories directly,
/// which is more thorough than plotters' font-kit backend; the
/// panic guard in chart.rs catches the rare case where fontdb
/// discovers a font that font-kit cannot rasterise.
fn resolve_font_family(config: &StatsConfig) -> &str {
    RESOLVED_FAMILY.get_or_init(|| {
        if !config.font_family.is_empty() && is_font_available(&config.font_family) {
            return config.font_family.clone();
        }

        if !config.font_family.is_empty() {
            for &family in CJK_FALLBACK_FAMILIES {
                if is_font_available(family) {
                    warn!(
                        target: "Plugin/Stats",
                        "Font '{}' not found, using fallback '{}'",
                        config.font_family, family
                    );
                    return family.to_string();
                }
            }
        } else {
            for &family in CJK_FALLBACK_FAMILIES {
                if is_font_available(family) {
                    return family.to_string();
                }
            }
        }

        warn!(
            target: "Plugin/Stats",
            "No CJK font found; chart text may not render."
        );
        "sans-serif".to_string()
    })
}

pub fn get_font_family(config: &StatsConfig) -> &str {
    resolve_font_family(config)
}

// ================= 配色方案 =================

pub struct ColorScheme {
    pub background: RGBColor,
    pub card_background: RGBColor,
    pub primary: RGBColor,
    pub text_primary: RGBColor,
    pub text_secondary: RGBColor,
    pub grid_line: RGBColor,
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self {
            background: RGBColor(255, 255, 255),
            card_background: RGBColor(255, 255, 255),
            primary: RGBColor(59, 130, 246),
            text_primary: RGBColor(30, 41, 59),
            text_secondary: RGBColor(100, 116, 139),
            grid_line: RGBColor(226, 232, 240),
        }
    }
}

pub fn get_font<'a>(config: &'a StatsConfig, size: u32) -> TextStyle<'a> {
    let colors = ColorScheme::default();
    let family = get_font_family(config);
    (family, size).into_font().color(&colors.text_primary)
}

pub fn get_font_with_color<'a>(
    config: &'a StatsConfig,
    size: u32,
    color: &'a RGBColor,
) -> TextStyle<'a> {
    let family = get_font_family(config);
    (family, size).into_font().color(color)
}

pub fn get_contrast_color(bg_color: RGBColor) -> RGBColor {
    let (r, g, b) = (bg_color.0 as u32, bg_color.1 as u32, bg_color.2 as u32);
    // YIQ brightness formula
    let yiq = (r * 299 + g * 587 + b * 114) / 1000;
    if yiq >= 128 {
        RGBColor(0, 0, 0)
    } else {
        RGBColor(255, 255, 255)
    }
}

pub fn mix_with_white(color: RGBColor, opacity: f32) -> RGBColor {
    let r = (color.0 as f32 * opacity + 255.0 * (1.0 - opacity)) as u8;
    let g = (color.1 as f32 * opacity + 255.0 * (1.0 - opacity)) as u8;
    let b = (color.2 as f32 * opacity + 255.0 * (1.0 - opacity)) as u8;
    RGBColor(r, g, b)
}

pub fn save_rgba_to_base64(img: RgbaImage) -> Result<String, String> {
    let dynamic_image = DynamicImage::ImageRgba8(img);
    let mut cursor = std::io::Cursor::new(Vec::new());
    dynamic_image
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|e| format!("图片编码失败: {}", e))?;
    let b64 = general_purpose::STANDARD.encode(cursor.into_inner());
    Ok(format!("base64://{}", b64))
}

pub fn truncate_text_to_fit(
    font: &plotters::style::FontDesc,
    text: &str,
    max_width: u32,
) -> String {
    let (w, _) = font.box_size(text).unwrap_or((0, 0));
    if w <= max_width {
        return text.to_string();
    }

    let mut s = text.to_string();
    while !s.is_empty() {
        s.pop();
        let candidate = format!("{}...", s);
        let (w, _) = font.box_size(&candidate).unwrap_or((0, 0));
        if w <= max_width {
            return candidate;
        }
    }
    "...".to_string()
}

pub fn get_average_color(img: &RgbaImage) -> RGBColor {
    let mut r_sum = 0u64;
    let mut g_sum = 0u64;
    let mut b_sum = 0u64;
    let count = (img.width() * img.height()) as u64;

    if count == 0 {
        return RGBColor(59, 130, 246);
    }

    for p in img.pixels() {
        r_sum += p[0] as u64;
        g_sum += p[1] as u64;
        b_sum += p[2] as u64;
    }

    RGBColor(
        (r_sum / count) as u8,
        (g_sum / count) as u8,
        (b_sum / count) as u8,
    )
}

pub fn make_circular_avatar(img: &DynamicImage, size: u32) -> RgbaImage {
    let rgba = img.to_rgba8();
    let mut result = RgbaImage::new(size, size);
    let center = size as f32 / 2.0;
    let radius = center - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center + 0.5;
            let dy = y as f32 - center + 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius - 0.5 {
                result.put_pixel(x, y, *rgba.get_pixel(x, y));
            } else if dist <= radius + 0.5 {
                let alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
                let mut pixel = *rgba.get_pixel(x, y);
                pixel[3] = (pixel[3] as f32 * alpha) as u8;
                result.put_pixel(x, y, pixel);
            }
        }
    }
    result
}

pub fn create_default_avatar(size: u32) -> RgbaImage {
    let mut result = RgbaImage::new(size, size);
    let center = size as f32 / 2.0;
    let radius = center - 1.0;
    let bg_color = Rgba([200, 200, 200, 255]);

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center + 0.5;
            let dy = y as f32 - center + 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius - 0.5 {
                result.put_pixel(x, y, bg_color);
            } else if dist <= radius + 0.5 {
                let alpha = (radius + 0.5 - dist).clamp(0.0, 1.0);
                let mut pixel = bg_color;
                pixel[3] = (255.0 * alpha) as u8;
                result.put_pixel(x, y, pixel);
            }
        }
    }
    result
}

pub fn overlay_image(base: &mut RgbaImage, overlay: &RgbaImage, x: i32, y: i32) {
    let (base_w, base_h) = base.dimensions();
    let (overlay_w, overlay_h) = overlay.dimensions();

    for oy in 0..overlay_h {
        for ox in 0..overlay_w {
            let bx = x + ox as i32;
            let by = y + oy as i32;

            if bx >= 0 && bx < base_w as i32 && by >= 0 && by < base_h as i32 {
                let bg = base.get_pixel(bx as u32, by as u32);
                let fg = overlay.get_pixel(ox, oy);

                let alpha = fg[3] as f32 / 255.0;
                if alpha > 0.0 {
                    let blended = Rgba([
                        ((1.0 - alpha) * bg[0] as f32 + alpha * fg[0] as f32) as u8,
                        ((1.0 - alpha) * bg[1] as f32 + alpha * fg[1] as f32) as u8,
                        ((1.0 - alpha) * bg[2] as f32 + alpha * fg[2] as f32) as u8,
                        255,
                    ]);
                    base.put_pixel(bx as u32, by as u32, blended);
                }
            }
        }
    }
}
