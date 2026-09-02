//! 词意卡片的原生绘制画布。
//!
//! 不依赖浏览器：文字用 `ab_glyph` 直接光栅化，几何图元用 SDF 逐像素判定。
//! 所有坐标都是「逻辑像素」（对齐原 HTML 设计稿），绘制时统一乘上设备比例
//! `s`，出图分辨率随 `image_scale` 走，字形边缘不会糊。

// 绘图原语（x/y/w/h/r/ink…）参数个数是自然的，不做结构体打包
#![allow(clippy::too_many_arguments)]

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgba, RgbaImage};

/// 带透明度的颜色。最终都往不透明底上 over 混合。
#[derive(Clone, Copy)]
pub struct Ink {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Ink {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Ink { r, g, b, a: 1.0 }
    }
    pub const fn with_a(self, a: f32) -> Self {
        Ink { a: a.clamp(0.0, 1.0), ..self }
    }
    /// `#RRGGBB`
    pub fn hex(hex: &str) -> Self {
        let h = hex.trim_start_matches('#');
        let n = u32::from_str_radix(h, 16).unwrap_or(0x888888);
        Ink::rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
    }
}

// ================= 字体 =================

/// 两个字族 × 两档字重：宋体管汉字与标题，黑体管数字与元信息；
/// Bold 用于标题与数字，Regular 用于正文。找不到对应字重时按
/// 「同族 Regular → 同字族另一档 → 黑体」逐级回退。
pub struct Fonts {
    pub serif_b: FontVec,
    pub serif: FontVec,
    pub sans_b: FontVec,
    pub sans: FontVec,
}

/// fontdb 的 `load_system_fonts` 不覆盖 Android / Termux，补上系统字体目录。
const EXTRA_FONT_DIRS: &[&str] = &[
    "/system/fonts",
    "/system/font",
    "/data/fonts",
    "/product/fonts",
    "/system/product/fonts",
];

/// 连族名都查不到时按文件兜底（Android 自带的 CJK 字体）。
/// `.ttc` 用 fontdb 给出的 face 序号取。
const SERIF_FILES: &[&str] = &[
    "/system/fonts/NotoSerifCJK-Bold.ttc",
    "/system/fonts/NotoSerifCJK-Regular.ttc",
    "/system/fonts/NotoSerifCJKsc-Bold.otf",
    "/system/fonts/NotoSerifCJKsc-Regular.otf",
];
const SANS_FILES: &[&str] = &[
    "/system/fonts/NotoSansCJK-Bold.ttc",
    "/system/fonts/NotoSansCJK-Regular.ttc",
    "/system/fonts/DroidSansFallbackFull.ttf",
    "/system/fonts/DroidSansFallback.ttf",
];

const SERIF_FAMILIES: &[&str] = &[
    "Noto Serif CJK SC",
    "Noto Serif SC",
    "Source Han Serif SC",
    "Source Han Serif CN",
    "Songti SC",
    "STSong",
    "SimSun",
];
const SANS_FAMILIES: &[&str] = &[
    "Noto Sans CJK SC",
    "Noto Sans SC",
    "Source Han Sans SC",
    "Source Han Sans CN",
    "PingFang SC",
    "Microsoft YaHei",
    "WenQuanYi Zen Hei",
    "WenQuanYi Micro Hei",
    "Droid Sans Fallback",
];

static FONTS: std::sync::OnceLock<Option<Fonts>> = std::sync::OnceLock::new();

impl Fonts {
    /// 进程内加载一次；环境里一个可用字体都没有时返回 None，调用方退回纯文本。
    pub fn get() -> Option<&'static Fonts> {
        FONTS.get_or_init(Fonts::load).as_ref()
    }

    fn load() -> Option<Fonts> {
        let db = load_db();
        let serif = |weight: fontdb::Weight| {
            load_family(&db, SERIF_FAMILIES, weight)
                .or_else(|| load_files(SERIF_FILES))
                .or_else(|| load_family(&db, SANS_FAMILIES, weight))
                .or_else(|| load_files(SANS_FILES))
        };
        let sans = |weight: fontdb::Weight| {
            load_family(&db, SANS_FAMILIES, weight)
                .or_else(|| load_files(SANS_FILES))
                .or_else(|| load_family(&db, SERIF_FAMILIES, weight))
                .or_else(|| load_files(SERIF_FILES))
        };
        let serif_b = serif(fontdb::Weight::BOLD)?;
        let serif = serif(fontdb::Weight::NORMAL)?;
        let sans_b = sans(fontdb::Weight::BOLD)?;
        let sans = sans(fontdb::Weight::NORMAL)?;
        Some(Fonts { serif_b, serif, sans_b, sans })
    }
}

fn load_db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    for dir in EXTRA_FONT_DIRS {
        if std::path::Path::new(dir).is_dir() {
            db.load_fonts_dir(dir);
        }
    }
    if let Ok(prefix) = std::env::var("PREFIX") {
        db.load_fonts_dir(std::path::Path::new(&prefix).join("share/fonts"));
    }
    if let Ok(home) = std::env::var("HOME") {
        db.load_fonts_dir(std::path::Path::new(&home).join(".fonts"));
        db.load_fonts_dir(std::path::Path::new(&home).join(".local/share/fonts"));
    }
    db
}

fn face_from(db: &fontdb::Database, id: fontdb::ID) -> Option<FontVec> {
    db.with_face_data(id, |data, idx| {
        FontVec::try_from_vec_and_index(data.to_vec(), idx).ok()
    })?
}

/// 按族名与字重加载字体。
fn load_family(db: &fontdb::Database, families: &[&str], weight: fontdb::Weight) -> Option<FontVec> {
    for &family in families {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight,
            ..Default::default()
        };
        if let Some(id) = db.query(&query)
            && let Some(f) = face_from(db, id)
        {
            return Some(f);
        }
    }
    None
}

/// 按文件路径兜底：Android 的系统字体没有可查询的 fontconfig 索引。
/// `.ttc` 直接取 0 号 face，够渲染中日韩汉字。
fn load_files(files: &[&str]) -> Option<FontVec> {
    for &file in files {
        if !std::path::Path::new(file).is_file() {
            continue;
        }
        if let Ok(data) = std::fs::read(file)
            && let Ok(f) = FontVec::try_from_vec_and_index(data, 0)
        {
            return Some(f);
        }
    }
    None
}

// ================= 画布 =================

/// 逻辑坐标画布：内部图像是逻辑尺寸 × `s`。
pub struct Canvas {
    pub img: RgbaImage,
    s: f32,
}

impl Canvas {
    pub fn new(w_logical: f32, h_logical: f32, s: f32) -> Self {
        let img = RgbaImage::new(
            (w_logical * s).ceil() as u32,
            (h_logical * s).ceil() as u32,
        );
        Canvas { img, s }
    }

    pub fn s(&self) -> f32 {
        self.s
    }

    fn blend(&mut self, x: i32, y: i32, ink: Ink, cov: f32) {
        let (w, h) = self.img.dimensions();
        if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
            return;
        }
        let a = (ink.a * cov).clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let p = self.img.get_pixel_mut(x as u32, y as u32);
        // 标准 over 合成（straight alpha）：底为透明时保留墨色本来的透明度，
        // 图层上的半透明墨色才不会混进黑色背景
        let pa = p[3] as f32 / 255.0;
        let oa = a + pa * (1.0 - a);
        let mix = |fg: u8, bg: u8| -> u8 {
            if oa <= 0.0 { 0 } else { ((fg as f32 * a + bg as f32 * pa * (1.0 - a)) / oa) as u8 }
        };
        *p = Rgba([
            mix(ink.r, p[0]),
            mix(ink.g, p[1]),
            mix(ink.b, p[2]),
            (oa * 255.0) as u8,
        ]);
    }

    /// 整幅填充（背景）
    pub fn fill(&mut self, ink: Ink) {
        let (w, h) = self.img.dimensions();
        for y in 0..h {
            for x in 0..w {
                self.blend(x as i32, y as i32, ink, 1.0);
            }
        }
    }

    /// 实心矩形（逻辑坐标，下同）
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, ink: Ink) {
        let s = self.s;
        let x0 = (x * s).floor() as i32;
        let y0 = (y * s).floor() as i32;
        let x1 = ((x + w) * s).ceil() as i32;
        let y1 = ((y + h) * s).ceil() as i32;
        for py in y0..y1 {
            for px in x0..x1 {
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 圆角矩形 SDF：`d<=0` 在内部。`mode` 控制填充实心还是描边。
    fn rrect_sdf(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, ink: Ink, stroke: Option<f32>) {
        let s = self.s;
        let (hw, hh) = (w * s / 2.0, h * s / 2.0);
        let cx = (x * s) + hw;
        let cy = (y * s) + hh;
        let r = (r * s).min(hw).min(hh).max(0.0);
        let sw = stroke.unwrap_or(0.0) * s;

        let x0 = (cx - hw - sw - 1.0).floor() as i32;
        let x1 = (cx + hw + sw + 1.0).ceil() as i32;
        let y0 = (cy - hh - sw - 1.0).floor() as i32;
        let y1 = (cy + hh + sw + 1.0).ceil() as i32;

        for py in y0.max(0)..y1.min(self.img.height() as i32) {
            for px in x0.max(0)..x1.min(self.img.width() as i32) {
                let qx = (px as f32 - cx).abs() - (hw - r);
                let qy = (py as f32 - cy).abs() - (hh - r);
                let d_out = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - r;
                // 边缘 1px 覆盖度渐变，近似抗锯齿
                let cov = match stroke {
                    None => (0.5 - d_out).clamp(0.0, 1.0),
                    Some(_) => (sw / 2.0 + 0.5 - d_out.abs()).clamp(0.0, 1.0),
                };
                if cov > 0.0 {
                    self.blend(px, py, ink, cov);
                }
            }
        }
    }

    pub fn rrect_fill(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, ink: Ink) {
        self.rrect_sdf(x, y, w, h, r, ink, None);
    }

    pub fn rrect_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, stroke_px: f32, ink: Ink) {
        self.rrect_sdf(x, y, w, h, r, ink, Some(stroke_px));
    }

    pub fn circle_fill(&mut self, cx: f32, cy: f32, r: f32, ink: Ink) {
        let s = self.s;
        let (cx, cy, r) = (cx * s, cy * s, r * s);
        for py in (cy - r - 1.0).floor() as i32..(cy + r + 1.0).ceil() as i32 {
            for px in (cx - r - 1.0).floor() as i32..(cx + r + 1.0).ceil() as i32 {
                let d = (px as f32 - cx).hypot(py as f32 - cy) - r;
                if d <= 0.0 {
                    self.blend(px, py, ink, (0.5 - d).clamp(0.0, 1.0));
                }
            }
        }
    }

    pub fn circle_stroke(&mut self, cx: f32, cy: f32, r: f32, stroke_px: f32, ink: Ink) {
        let s = self.s;
        let (cx, cy, r, sw) = (cx * s, cy * s, r * s, stroke_px * s);
        for py in (cy - r - sw - 1.0).floor() as i32..(cy + r + sw + 1.0).ceil() as i32 {
            for px in (cx - r - sw - 1.0).floor() as i32..(cx + r + sw + 1.0).ceil() as i32 {
                let d = ((px as f32 - cx).hypot(py as f32 - cy) - r).abs();
                let cov = (sw / 2.0 + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(px, py, ink, cov);
                }
            }
        }
    }

    /// 横线。`dash`/`gap` 给定时画虚线（dash 段 + gap 空），全 0 为实线。
    pub fn hline(&mut self, x0: f32, x1: f32, y: f32, thick: f32, ink: Ink, dash: f32, gap: f32) {
        let s = self.s;
        let (ax, bx) = ((x0.min(x1)) * s, (x0.max(x1)) * s);
        let (ty, by) = (y * s, (y + thick) * s);
        let py0 = ty.floor() as i32;
        let py1 = by.ceil() as i32;
        let period = if dash > 0.0 { dash + gap } else { 0.0 };
        for px in (ax.floor() as i32)..(bx.ceil() as i32) {
            if period > 0.0 {
                let t = px as f32 - ax;
                if t % period >= dash {
                    continue;
                }
            }
            for py in py0..py1 {
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 竖线
    pub fn vline(&mut self, x: f32, y0: f32, y1: f32, thick: f32, ink: Ink) {
        let s = self.s;
        let (ay, by) = ((y0.min(y1)) * s, (y0.max(y1)) * s);
        let (tx, bx) = (x * s, (x + thick) * s);
        for px in (tx.floor() as i32)..(bx.ceil() as i32) {
            for py in (ay.floor() as i32)..(by.ceil() as i32) {
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 竖虚线（田字格用）
    pub fn vdash(&mut self, x: f32, y0: f32, y1: f32, thick: f32, ink: Ink, dash: f32, gap: f32) {
        let s = self.s;
        let (ay, by) = ((y0.min(y1)) * s, (y0.max(y1)) * s);
        let period = if dash > 0.0 { dash + gap } else { 0.0 };
        let px0 = (x * s).floor() as i32;
        let px1 = ((x + thick) * s).ceil() as i32;
        for py in (ay.floor() as i32)..(by.ceil() as i32) {
            if period > 0.0 {
                let t = py as f32 - ay;
                if t % period >= dash {
                    continue;
                }
            }
            for px in px0..px1 {
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 质感点阵：在矩形区域内按 `step` 网格画小圆点。
    pub fn dot_grid(&mut self, x0: f32, y0: f32, w: f32, h: f32, step: f32, ox: f32, oy: f32, radius: f32, ink: Ink) {
        let mut gy = y0 + oy;
        while gy < y0 + h {
            let mut gx = x0 + ox;
            while gx < x0 + w {
                self.circle_fill(gx, gy, radius, ink);
                gx += step;
            }
            gy += step;
        }
    }

    // ================= 文字 =================

    /// 文字宽度（逻辑像素）
    pub fn text_w(&self, text: &str, font: &FontVec, px: f32, spacing: f32) -> f32 {
        let sc = font.as_scaled(PxScale::from(px * self.s));
        let mut w = 0.0f32;
        let n = text.chars().count();
        for (i, ch) in text.chars().enumerate() {
            w += sc.h_advance(sc.glyph_id(ch)) / self.s;
            if i + 1 < n {
                w += spacing;
            }
        }
        w
    }

    /// 文字行高（逻辑像素）
    pub fn text_h(&self, font: &FontVec, px: f32) -> f32 {
        let sc = font.as_scaled(PxScale::from(px * self.s));
        (sc.ascent() - sc.descent()) / self.s
    }

    /// 在基线处绘制一行文字，返回占用宽度。坐标为逻辑像素。
    pub fn text(&mut self, x: f32, baseline: f32, text: &str, font: &FontVec, px: f32, ink: Ink, spacing: f32) -> f32 {
        let s = self.s;
        let scale = PxScale::from(px * s);
        let sc = font.as_scaled(scale);
        let mut pen = x * s;
        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            let glyph = gid.with_scale_and_position(scale, point(pen, baseline * s));
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                let (bx, by) = (b.min.x as i32, b.min.y as i32);
                og.draw(|gx, gy, cov| {
                    self.blend(bx + gx as i32, by + gy as i32, ink, cov);
                });
            }
            pen += sc.h_advance(gid) + spacing * s;
        }
        (pen - x * s - if text.is_empty() { 0.0 } else { spacing * s }) / s
    }

    /// 水平垂直居中绘制
    pub fn text_center(&mut self, cx: f32, cy: f32, text: &str, font: &FontVec, px: f32, ink: Ink, spacing: f32) {
        let w = self.text_w(text, font, px, spacing);
        let sc = font.as_scaled(PxScale::from(px * self.s));
        // top = cy - 行高/2；baseline = top + ascent（均换算回逻辑像素）
        let baseline = cy - (sc.ascent() - sc.descent()) / 2.0 / self.s + sc.ascent() / self.s;
        self.text(cx - w / 2.0, baseline, text, font, px, ink, spacing);
    }

    /// 右对齐绘制
    pub fn text_right(&mut self, rx: f32, baseline: f32, text: &str, font: &FontVec, px: f32, ink: Ink, spacing: f32) {
        let w = self.text_w(text, font, px, spacing);
        self.text(rx - w, baseline, text, font, px, ink, spacing);
    }

    /// 超宽截断加省略号，返回截断后的文本
    pub fn ellipsis(&self, text: &str, font: &FontVec, px: f32, spacing: f32, max_w: f32) -> String {
        if self.text_w(text, font, px, spacing) <= max_w {
            return text.to_string();
        }
        let mut s: String = text.chars().take_while(|_| true).collect();
        while !s.is_empty() {
            s.pop();
            let cand = format!("{s}…");
            if self.text_w(&cand, font, px, spacing) <= max_w {
                return cand;
            }
        }
        "…".to_string()
    }

    /// 按宽度折行，最多 `max_lines` 行；放不下时末行加省略号。
    pub fn wrap(&self, text: &str, font: &FontVec, px: f32, spacing: f32, max_w: f32, max_lines: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut cur = String::new();
        for ch in text.chars() {
            cur.push(ch);
            if self.text_w(&cur, font, px, spacing) > max_w {
                cur.pop();
                if lines.len() + 1 < max_lines {
                    lines.push(std::mem::take(&mut cur));
                } else {
                    // 最后一行：截断 + 省略号
                    while !cur.is_empty() && self.text_w(&format!("{cur}…"), font, px, spacing) > max_w {
                        cur.pop();
                    }
                    cur.push('…');
                    break;
                }
                cur.push(ch);
            }
        }
        if !cur.is_empty() || lines.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// 把一层小图绕自身中心旋转后叠到画布上（印章、旗标用）。
    /// 双线性采样，避免最近邻在小角度旋转下的重影。
    pub fn blit_rotated(&mut self, layer: &RgbaImage, cx: f32, cy: f32, angle_deg: f32) {
        let s = self.s;
        let rad = angle_deg.to_radians();
        let (sin, cos) = (rad.sin(), rad.cos());
        let (lw, lh) = (layer.width() as f32, layer.height() as f32);
        let half = lw.max(lh) / 2.0 * std::f32::consts::SQRT_2 + 2.0;
        let (dcx, dcy) = (cx * s, cy * s);

        for py in (dcy - half).floor() as i32..(dcy + half).ceil() as i32 {
            for px in (dcx - half).floor() as i32..(dcx + half).ceil() as i32 {
                // 反向旋转采样源坐标
                let dx = px as f32 - dcx;
                let dy = py as f32 - dcy;
                let sx = dx * cos + dy * sin + lw / 2.0;
                let sy = -dx * sin + dy * cos + lh / 2.0;
                if sx < -1.0 || sy < -1.0 || sx >= lw || sy >= lh {
                    continue;
                }
                // 双线性插值四个相邻像素（越界按透明处理）；
                // 先预乘 alpha 再累加，避免透明黑邻像素把颜色拉暗
                let (x0, y0) = (sx.floor() as i32, sy.floor() as i32);
                let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                let mut acc = [0.0f32; 4];
                for (i, (ox, oy)) in [(0, 0), (1, 0), (0, 1), (1, 1)].iter().enumerate() {
                    let (x, y) = (x0 + ox, y0 + oy);
                    let w = if i & 1 == 0 { 1.0 - fx } else { fx }
                        * if i & 2 == 0 { 1.0 - fy } else { fy };
                    let p = if x < 0 || y < 0 || x >= lw as i32 || y >= lh as i32 {
                        [0u8; 4]
                    } else {
                        layer.get_pixel(x as u32, y as u32).0
                    };
                    let a = p[3] as f32 / 255.0;
                    acc[0] += p[0] as f32 * a * w;
                    acc[1] += p[1] as f32 * a * w;
                    acc[2] += p[2] as f32 * a * w;
                    acc[3] += a * w;
                }
                if acc[3] <= 0.001 {
                    continue;
                }
                let ink = Ink {
                    r: (acc[0] / acc[3]) as u8,
                    g: (acc[1] / acc[3]) as u8,
                    b: (acc[2] / acc[3]) as u8,
                    a: acc[3].clamp(0.0, 1.0),
                };
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 裁剪到指定逻辑高度并返回图像
    pub fn crop(self, h_logical: f32) -> RgbaImage {
        let h = ((h_logical * self.s).ceil() as u32).min(self.img.height());
        let mut out = RgbaImage::new(self.img.width(), h);
        for y in 0..h {
            for x in 0..self.img.width() {
                out.put_pixel(x, y, *self.img.get_pixel(x, y));
            }
        }
        out
    }
}

