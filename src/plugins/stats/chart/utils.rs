use crate::plugins::stats::StatsConfig;
use base64::{Engine as _, engine::general_purpose};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use plotters::prelude::*;
use plotters::style::{FontStyle, register_font};
use std::path::Path;
use std::sync::OnceLock;

// ================= 字体加载 =================

/// 注册到 plotters 的内部字体名。使用固定名称避免与系统字体族名冲突 —
/// 不论用户提供路径还是字体族，最终绘制时都查找该名称即可命中我们注册的字节。
const CHART_FONT_NAME: &str = "ChartFont";

/// 当用户未指定字体或指定的字体不可用时按顺序尝试的 CJK 回退字体。
const CJK_FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Sans SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK TC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "Source Han Sans",
    "WenQuanYi Micro Hei",
    "WenQuanYi Zen Hei",
    "Microsoft YaHei",
    "SimHei",
    "SimSun",
    "PingFang SC",
    "Heiti SC",
    "STHeiti",
    "Arial Unicode MS",
];

static FONT_DB: OnceLock<fontdb::Database> = OnceLock::new();
static RESOLVED_FONT: OnceLock<String> = OnceLock::new();

fn get_font_db() -> &'static fontdb::Database {
    FONT_DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// 将字节注册到 plotters 的 ab_glyph 后端。注册成功返回 true。
/// 注意：plotters 需要 `'static` 字节切片，因此我们 leak 一次（每进程最多发生一次）。
fn register_bytes(bytes: Vec<u8>) -> bool {
    let static_bytes: &'static [u8] = bytes.leak();
    register_font(CHART_FONT_NAME, FontStyle::Normal, static_bytes).is_ok()
}

/// 从文件路径加载字体并注册。
fn try_load_path(path: &str) -> bool {
    let p = Path::new(path);
    if !p.is_file() {
        warn!(target: "Plugin/Stats", "字体路径不存在或不是文件: {}", path);
        return false;
    }
    match std::fs::read(p) {
        Ok(bytes) => {
            if register_bytes(bytes) {
                info!(target: "Plugin/Stats", "已加载字体文件: {}", path);
                true
            } else {
                warn!(target: "Plugin/Stats", "字体文件无法被解析: {}", path);
                false
            }
        }
        Err(e) => {
            warn!(target: "Plugin/Stats", "字体文件读取失败 {}: {}", path, e);
            false
        }
    }
}

/// 通过 fontdb 在系统字体中查找 family 并注册。
fn try_load_family(family: &str) -> bool {
    let db = get_font_db();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..Default::default()
    };
    let id = match db.query(&query) {
        Some(id) => id,
        None => return false,
    };
    let bytes: Option<Vec<u8>> = db.with_face_data(id, |data, _idx| data.to_vec());
    match bytes {
        Some(b) => register_bytes(b),
        None => false,
    }
}

/// 加载并注册字体，优先级：
/// 1. `font_path`（路径优先，绕过任何系统字体发现）
/// 2. `font_family`（通过 fontdb 在系统字体中查找）
/// 3. CJK 回退字体列表
/// 4. 失败时返回 "sans-serif"，由 plotters 兜底处理
fn resolve_font(config: &StatsConfig) -> &str {
    RESOLVED_FONT.get_or_init(|| {
        let path = config.font_path.trim();
        let family = config.font_family.trim();

        // 1. 路径优先
        if !path.is_empty() && try_load_path(path) {
            return CHART_FONT_NAME.to_string();
        }

        // 2. 字体族
        if !family.is_empty() {
            if try_load_family(family) {
                info!(target: "Plugin/Stats", "已加载字体族: {}", family);
                return CHART_FONT_NAME.to_string();
            }
            warn!(
                target: "Plugin/Stats",
                "字体族 '{}' 在系统中不可用, 尝试 CJK 回退字体",
                family
            );
        }

        // 3. CJK 回退
        for &fb in CJK_FALLBACK_FAMILIES {
            // 跳过用户已尝试过的 family，避免重复警告
            if !family.is_empty() && fb.eq_ignore_ascii_case(family) {
                continue;
            }
            if try_load_family(fb) {
                warn!(target: "Plugin/Stats", "使用回退字体族: {}", fb);
                return CHART_FONT_NAME.to_string();
            }
        }

        warn!(
            target: "Plugin/Stats",
            "未找到任何可用 CJK 字体，图表中的中文可能无法渲染。请通过 font_path 指定字体文件，或安装对应字体族。"
        );
        "sans-serif".to_string()
    })
}

pub fn get_font_family(config: &StatsConfig) -> &str {
    resolve_font(config)
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

/// 向白以外的底色混合：`opacity=1` 保留原色，`0` 变为 `base`。
pub fn mix_with_color(color: RGBColor, base: RGBColor, opacity: f32) -> RGBColor {
    let t = opacity.clamp(0.0, 1.0);
    let r = (color.0 as f32 * t + base.0 as f32 * (1.0 - t)) as u8;
    let g = (color.1 as f32 * t + base.1 as f32 * (1.0 - t)) as u8;
    let b = (color.2 as f32 * t + base.2 as f32 * (1.0 - t)) as u8;
    RGBColor(r, g, b)
}

/// 用矩形 + 四角圆近似填充圆角矩形（plotters 无原生圆角）。
pub fn draw_rounded_rect<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    color: RGBColor,
) -> Result<(), String> {
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let max_r = ((x1 - x0).min(y1 - y0) / 2).max(0);
    let r = radius.clamp(0, max_r);

    if r == 0 {
        root.draw(&Rectangle::new([(x0, y0), (x1, y1)], color.filled()))
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    root.draw(&Rectangle::new(
        [(x0 + r, y0), (x1 - r, y1)],
        color.filled(),
    ))
    .map_err(|e| e.to_string())?;
    root.draw(&Rectangle::new(
        [(x0, y0 + r), (x1, y1 - r)],
        color.filled(),
    ))
    .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x0 + r, y0 + r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x1 - r, y0 + r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x0 + r, y1 - r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x1 - r, y1 - r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 左侧色条：左上/左下圆角，右侧平切，贴在圆角卡片内沿。
pub fn draw_left_accent_bar<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    color: RGBColor,
) -> Result<(), String> {
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let max_r = ((x1 - x0).min(y1 - y0) / 2).max(0);
    let r = radius.clamp(0, max_r);

    if r == 0 {
        root.draw(&Rectangle::new([(x0, y0), (x1, y1)], color.filled()))
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 主体：右侧平齐，不画右圆角
    root.draw(&Rectangle::new([(x0 + r, y0), (x1, y1)], color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Rectangle::new([(x0, y0 + r), (x0 + r, y1 - r)], color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x0 + r, y0 + r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    root.draw(&Circle::new((x0 + r, y1 - r), r, color.filled()))
        .map_err(|e| e.to_string())?;
    Ok(())
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
