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

/// 一张「字面」：字体本体 + 需要合成多少粗。
///
/// Android 自带的 Noto Serif/Sans CJK 只有 Regular 一档，查 Bold 查回来的
/// 还是那张 400 的脸——照着画出来的标题一律偏细偏虚。浏览器遇到这种情况会
/// 自己把轮廓外扩一圈（伪粗体），原生绘制这边同样得补上，否则卡片的层级
/// 全靠字号撑，一眼看过去是平的。[`Canvas::text`] 按 `embolden` 外扩字形。
pub struct Typeface {
    font: FontVec,
    /// 每侧外扩量，单位是「字号的倍数」；字体本身就有这一档字重时为 0。
    embolden: f32,
}

impl std::ops::Deref for Typeface {
    type Target = FontVec;
    fn deref(&self) -> &FontVec {
        &self.font
    }
}

/// 合成粗体的外扩量。取值参照浏览器：把轮廓描一圈 1/24 字号的线，
/// 摊到每一侧就是 1/48 ≈ 0.021 em。再多就糊成一团，再少看不出区别。
const SYNTHETIC_BOLD: f32 = 0.021;

/// 拿到的字重够不够粗？SemiBold（600）起算真粗体，再轻的都要自己补。
fn embolden_for(weight: fontdb::Weight) -> f32 {
    if weight.0 >= 600 { 0.0 } else { SYNTHETIC_BOLD }
}

/// 两个字族 × 两档字重：宋体管汉字与标题，黑体管数字与元信息；
/// Bold 用于标题与数字，Regular 用于正文。找不到对应字重时按
/// 「同族 Regular → 同字族另一档 → 黑体」逐级回退，字重不够就合成。
pub struct Fonts {
    pub serif_b: Typeface,
    pub serif: Typeface,
    pub sans_b: Typeface,
    pub sans: Typeface,
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
        // 「查 Bold 查回来的还是 Regular」在 Android 上是常态，不是异常：
        // 拿不到真字重就记下来，落笔时把字形外扩一圈补上。
        let bold = |hit: Option<(FontVec, fontdb::Weight)>| {
            hit.map(|(font, weight)| Typeface {
                font,
                embolden: embolden_for(weight),
            })
        };
        let plain = |hit: Option<(FontVec, fontdb::Weight)>| {
            hit.map(|(font, _)| Typeface { font, embolden: 0.0 })
        };
        Some(Fonts {
            serif_b: bold(serif(fontdb::Weight::BOLD))?,
            serif: plain(serif(fontdb::Weight::NORMAL))?,
            sans_b: bold(sans(fontdb::Weight::BOLD))?,
            sans: plain(sans(fontdb::Weight::NORMAL))?,
        })
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

/// 按族名与字重加载字体，并带回**实际拿到的**字重——查询是「最接近匹配」，
/// 要了 Bold 未必给 Bold，差额由合成粗体补。
fn load_family(
    db: &fontdb::Database,
    families: &[&str],
    weight: fontdb::Weight,
) -> Option<(FontVec, fontdb::Weight)> {
    for &family in families {
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight,
            ..Default::default()
        };
        if let Some(id) = db.query(&query)
            && let Some(f) = face_from(db, id)
        {
            let got = db.face(id).map_or(fontdb::Weight::NORMAL, |face| face.weight);
            return Some((f, got));
        }
    }
    None
}

/// 按文件路径兜底：Android 的系统字体没有可查询的 fontconfig 索引。
/// `.ttc` 直接取 0 号 face，够渲染中日韩汉字。文件名里带 Bold 才算真粗。
fn load_files(files: &[&str]) -> Option<(FontVec, fontdb::Weight)> {
    for &file in files {
        if !std::path::Path::new(file).is_file() {
            continue;
        }
        if let Ok(data) = std::fs::read(file)
            && let Ok(f) = FontVec::try_from_vec_and_index(data, 0)
        {
            let weight = if file.contains("Bold") {
                fontdb::Weight::BOLD
            } else {
                fontdb::Weight::NORMAL
            };
            return Some((f, weight));
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

    // ---------- 虚线路径 ----------

    /// 沿一条折线画虚线，返回前先把覆盖度收进蒙版，最后只落一次笔。
    ///
    /// 为什么要蒙版：虚线是用圆头笔触沿弧长密集点出来的，笔触必然互相重叠，
    /// 而半透明墨色每叠一次就深一层——直接落笔的话，线会比要的浓好几倍，
    /// 拐角处更是糊成一坨。取最大值再一次性混合，浓淡才是均匀的一条线。
    ///
    /// `closed` 为真时首尾相接，并把虚线周期微调成整数个，避免起点处出现接缝。
    fn dashed_path(&mut self, pts: &[(f32, f32)], closed: bool, thick: f32, ink: Ink, dash: f32, gap: f32) {
        if pts.len() < 2 || dash <= 0.0 {
            return;
        }
        let s = self.s;
        let radius = (thick * s / 2.0).max(0.5);

        // 逐段长度与总长（设备像素）
        let mut seg = Vec::with_capacity(pts.len());
        let last = if closed { pts.len() } else { pts.len() - 1 };
        let mut total = 0.0f32;
        for i in 0..last {
            let (ax, ay) = pts[i];
            let (bx, by) = pts[(i + 1) % pts.len()];
            let len = ((bx - ax) * s).hypot((by - ay) * s);
            seg.push(((ax * s, ay * s), (bx * s, by * s), len));
            total += len;
        }
        if total <= 0.0 {
            return;
        }

        // 闭合路径把周期凑成整数份，起点与终点的虚线才接得上
        let (dash, gap) = {
            let (d, g) = (dash * s, gap * s);
            if closed {
                let n = (total / (d + g)).round().max(1.0);
                let k = total / n / (d + g);
                (d * k, g * k)
            } else {
                (d, g)
            }
        };
        let period = dash + gap;

        // 蒙版范围
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(px, py) in pts {
            x0 = x0.min(px * s);
            y0 = y0.min(py * s);
            x1 = x1.max(px * s);
            y1 = y1.max(py * s);
        }
        let pad = radius.ceil() + 1.0;
        let (mx0, my0) = ((x0 - pad).floor() as i32, (y0 - pad).floor() as i32);
        let (mw, mh) = (
            ((x1 + pad).ceil() as i32 - mx0).max(1) as usize,
            ((y1 + pad).ceil() as i32 - my0).max(1) as usize,
        );
        let mut mask = vec![0f32; mw * mh];

        // 按弧长行进，落在 dash 段里就点一笔圆头
        let step = 0.4f32;
        let mut walked = 0.0f32;
        for ((ax, ay), (bx, by), len) in seg {
            if len <= 0.0 {
                continue;
            }
            let (ux, uy) = ((bx - ax) / len, (by - ay) / len);
            let mut t = 0.0f32;
            while t < len {
                if (walked + t) % period < dash {
                    stamp_disc(&mut mask, mw, mh, ax + ux * t - mx0 as f32, ay + uy * t - my0 as f32, radius);
                }
                t += step;
            }
            walked += len;
        }

        for my in 0..mh {
            for mx in 0..mw {
                let cov = mask[my * mw + mx];
                if cov > 0.0 {
                    self.blend(mx0 + mx as i32, my0 + my as i32, ink, cov);
                }
            }
        }
    }

    /// 虚线圆角矩形：留白牌、田字格、空盘占位框都用它。
    pub fn rrect_dashed(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, thick: f32, ink: Ink, dash: f32, gap: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let mut pts = Vec::with_capacity(40);
        // 四个圆角的圆心，配上各自的起始角（顺时针从右上角开始）
        let corners = [
            (x + w - r, y + r, -90.0f32),
            (x + w - r, y + h - r, 0.0),
            (x + r, y + h - r, 90.0),
            (x + r, y + r, 180.0),
        ];
        for (cx, cy, a0) in corners {
            for i in 0..=8 {
                let a = (a0 + 90.0 * i as f32 / 8.0).to_radians();
                pts.push((cx + r * a.cos(), cy + r * a.sin()));
            }
        }
        self.dashed_path(&pts, true, thick, ink, dash, gap);
    }

    /// 虚线直段（田字格的十字、行间分隔）
    pub fn dashed_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thick: f32, ink: Ink, dash: f32, gap: f32) {
        self.dashed_path(&[(x0, y0), (x1, y1)], false, thick, ink, dash, gap);
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
    pub fn text_w(&self, text: &str, font: &Typeface, px: f32, spacing: f32) -> f32 {
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
    pub fn text_h(&self, font: &Typeface, px: f32) -> f32 {
        let sc = font.as_scaled(PxScale::from(px * self.s));
        (sc.ascent() - sc.descent()) / self.s
    }

    /// 把一个字形加粗后落笔：先把覆盖度收进缓冲，再做半径 `grow` 的形态学膨胀。
    ///
    /// 不用「同一个字形错开位置画几遍」那种土办法——半透明墨色叠加会在重合处
    /// 变深，笔画交叉的地方就糊了。取窗口最大值等于把轮廓整体外推一圈，
    /// 边缘的抗锯齿渐变原样保留，最后只落一次笔，浓淡才均匀。
    fn draw_glyph_bold(
        &mut self,
        og: &ab_glyph::OutlinedGlyph,
        bx: i32,
        by: i32,
        grow: usize,
        ink: Ink,
    ) {
        let b = og.px_bounds();
        let gw = b.width().ceil() as usize + 1;
        let gh = b.height().ceil() as usize + 1;
        let (w, h) = (gw + 2 * grow, gh + 2 * grow);
        let mut src = vec![0f32; w * h];
        og.draw(|gx, gy, cov| {
            let (x, y) = (gx as usize + grow, gy as usize + grow);
            if x < w && y < h {
                let i = y * w + x;
                src[i] = src[i].max(cov);
            }
        });
        // 方形核可分离：先横后纵各取一次窗口最大值
        let mut tmp = vec![0f32; w * h];
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let mut m = 0f32;
                for k in x.saturating_sub(grow)..=(x + grow).min(w - 1) {
                    m = m.max(src[row + k]);
                }
                tmp[row + x] = m;
            }
        }
        for y in 0..h {
            for x in 0..w {
                let mut m = 0f32;
                for k in y.saturating_sub(grow)..=(y + grow).min(h - 1) {
                    m = m.max(tmp[k * w + x]);
                }
                if m > 0.0 {
                    self.blend(bx - grow as i32 + x as i32, by - grow as i32 + y as i32, ink, m);
                }
            }
        }
    }

    /// 在基线处绘制一行文字，返回占用宽度。坐标为逻辑像素。
    pub fn text(&mut self, x: f32, baseline: f32, text: &str, font: &Typeface, px: f32, ink: Ink, spacing: f32) -> f32 {
        let s = self.s;
        let scale = PxScale::from(px * s);
        let sc = font.as_scaled(scale);
        // 合成粗体的外扩半径（设备像素，取整）。字面本身够粗时为 0。
        let grow = (font.embolden * px * s).round() as usize;
        let mut pen = x * s;
        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            let glyph = gid.with_scale_and_position(scale, point(pen, baseline * s));
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                let (bx, by) = (b.min.x as i32, b.min.y as i32);
                if grow == 0 {
                    og.draw(|gx, gy, cov| {
                        self.blend(bx + gx as i32, by + gy as i32, ink, cov);
                    });
                } else {
                    self.draw_glyph_bold(&og, bx, by, grow, ink);
                }
            }
            // 外扩不改变步进：伪粗体只是把笔画描粗，字距还是原字体说了算，
            // 跟浏览器的做法一致，也让 text_w 量出来的宽度依然可信。
            pen += sc.h_advance(gid) + spacing * s;
        }
        (pen - x * s - if text.is_empty() { 0.0 } else { spacing * s }) / s
    }

    /// 水平垂直居中绘制
    pub fn text_center(&mut self, cx: f32, cy: f32, text: &str, font: &Typeface, px: f32, ink: Ink, spacing: f32) {
        let w = self.text_w(text, font, px, spacing);
        let sc = font.as_scaled(PxScale::from(px * self.s));
        // top = cy - 行高/2；baseline = top + ascent（均换算回逻辑像素）
        let baseline = cy - (sc.ascent() - sc.descent()) / 2.0 / self.s + sc.ascent() / self.s;
        self.text(cx - w / 2.0, baseline, text, font, px, ink, spacing);
    }

    /// 右对齐绘制
    pub fn text_right(&mut self, rx: f32, baseline: f32, text: &str, font: &Typeface, px: f32, ink: Ink, spacing: f32) {
        let w = self.text_w(text, font, px, spacing);
        self.text(rx - w, baseline, text, font, px, ink, spacing);
    }

    /// 超宽截断加省略号，返回截断后的文本
    pub fn ellipsis(&self, text: &str, font: &Typeface, px: f32, spacing: f32, max_w: f32) -> String {
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
    pub fn wrap(&self, text: &str, font: &Typeface, px: f32, spacing: f32, max_w: f32, max_lines: usize) -> Vec<String> {
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
    /// `ss` 是图层相对画布的超采样倍数（图层用 `Canvas::new(w, h, c.s() * ss)` 画）。
    /// 印章只转三五度，1 倍图层双线性重采样一次就把笔画的锐边磨圆了；
    /// 以 2 倍画好再缩下来，边缘才立得住。
    pub fn blit_rotated(&mut self, layer: &RgbaImage, cx: f32, cy: f32, angle_deg: f32, ss: f32) {
        let s = self.s;
        let ss = ss.max(1.0);
        let rad = angle_deg.to_radians();
        let (sin, cos) = (rad.sin(), rad.cos());
        let (lw, lh) = (layer.width() as f32, layer.height() as f32);
        let half = lw.max(lh) / ss / 2.0 * std::f32::consts::SQRT_2 + 2.0;
        let (dcx, dcy) = (cx * s, cy * s);

        // 双线性取一个源点，返回预乘 alpha 的 [r, g, b, a]；
        // 越界按透明处理，透明黑邻像素才不会把颜色拉暗。
        let sample = |sx: f32, sy: f32| -> [f32; 4] {
            if sx < -1.0 || sy < -1.0 || sx >= lw || sy >= lh {
                return [0.0; 4];
            }
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
            acc
        };

        // 超采样时目标像素取 2×2 个子样本，缩小的锯齿就压下去了
        let taps: &[(f32, f32)] = if ss > 1.0 {
            &[(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)]
        } else {
            &[(0.0, 0.0)]
        };

        for py in (dcy - half).floor() as i32..(dcy + half).ceil() as i32 {
            for px in (dcx - half).floor() as i32..(dcx + half).ceil() as i32 {
                let mut acc = [0.0f32; 4];
                for (tx, ty) in taps {
                    // 反向旋转采样源坐标；图层比画布密 ss 倍
                    let dx = px as f32 + tx - dcx;
                    let dy = py as f32 + ty - dcy;
                    let sx = (dx * cos + dy * sin) * ss + lw / 2.0;
                    let sy = (-dx * sin + dy * cos) * ss + lh / 2.0;
                    let t = sample(sx, sy);
                    for k in 0..4 {
                        acc[k] += t[k] / taps.len() as f32;
                    }
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


/// 往蒙版里点一个抗锯齿圆斑，取最大值——重叠处不加深。
fn stamp_disc(mask: &mut [f32], mw: usize, mh: usize, cx: f32, cy: f32, r: f32) {
    let x0 = ((cx - r - 1.0).floor() as i32).max(0) as usize;
    let x1 = ((cx + r + 1.0).ceil() as i32).clamp(0, mw as i32) as usize;
    let y0 = ((cy - r - 1.0).floor() as i32).max(0) as usize;
    let y1 = ((cy + r + 1.0).ceil() as i32).clamp(0, mh as i32) as usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
            let cov = (r + 0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                let i = y * mw + x;
                mask[i] = mask[i].max(cov);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SemiBold 起才算真粗体；再轻的字面都得自己补一圈，
    /// 否则在只带 Regular 中日韩字体的机器上，标题和正文一样细。
    #[test]
    fn only_a_genuinely_bold_face_skips_the_synthetic_pass() {
        assert_eq!(embolden_for(fontdb::Weight(400)), SYNTHETIC_BOLD);
        assert_eq!(embolden_for(fontdb::Weight(500)), SYNTHETIC_BOLD);
        assert_eq!(embolden_for(fontdb::Weight(600)), 0.0);
        assert_eq!(embolden_for(fontdb::Weight(700)), 0.0);
    }

    /// 虚线是用圆头笔触沿路径密集点出来的，笔触必然互相重叠。
    /// 蒙版取最大值就是为了让重叠处不加深——直接落笔的话，
    /// 半透明的线会一层层叠成实线，拐角尤其明显。
    #[test]
    fn overlapping_dash_strokes_never_darken_past_a_single_pass() {
        let alpha = 0.3;
        let mut c = Canvas::new(60.0, 40.0, 2.0);
        c.rrect_dashed(5.0, 5.0, 50.0, 30.0, 7.0, 1.5, Ink::rgb(0, 0, 0).with_a(alpha), 4.0, 4.0);
        let peak = c.img.pixels().map(|p| p[3]).max().unwrap_or(0) as f32 / 255.0;
        assert!(peak > alpha * 0.9, "虚线该画上去才对，实测峰值 {peak}");
        assert!(peak <= alpha + 0.02, "重叠处叠深了：峰值 {peak} 超过单次落笔的 {alpha}");
    }

    /// 合成粗体确实更粗：同一段字，粗体字面落下的墨必须明显多于常规字面。
    /// 机器上一个可用字体都没有时跳过——那种环境本来就走纯文本。
    #[test]
    fn the_bold_face_lays_down_more_ink_than_the_regular_one() {
        let Some(f) = Fonts::get() else {
            return;
        };
        let ink = |face: &Typeface| {
            let mut c = Canvas::new(200.0, 60.0, 2.0);
            c.text(10.0, 42.0, "词意东西", face, 32.0, Ink::rgb(0, 0, 0), 0.0);
            c.img.pixels().map(|p| p[3] as u64).sum::<u64>()
        };
        let (plain, bold) = (ink(&f.serif), ink(&f.serif_b));
        assert!(plain > 0, "常规字面应当画出字来");
        if f.serif_b.embolden > 0.0 {
            assert!(
                bold as f64 >= plain as f64 * 1.15,
                "合成粗体该明显更重：常规 {plain}，粗体 {bold}"
            );
        }
    }

    /// 报告四张字面各自落到了哪个字体文件、拿到的是哪一档字重。
    ///
    /// 「宋体没生效」「标题怎么是细的」这类问题，靠肉眼看小字号的图判不出来
    /// （40px 上宋体与黑体的汉字几乎分辨不出），得让加载器自己说。
    ///   cargo test ciyi::painter::tests::report -- --ignored --nocapture
    #[test]
    #[ignore = "环境诊断，输出取决于本机字体"]
    fn report_resolved_font_files() {
        let db = load_db();
        for (label, families) in [("serif", SERIF_FAMILIES), ("sans", SANS_FAMILIES)] {
            for weight in [fontdb::Weight::NORMAL, fontdb::Weight::BOLD] {
                let got = load_family(&db, families, weight).map(|(_, w)| w);
                // 只有粗体那一档才会去合成；常规档拿到什么就用什么
                let synth = if weight == fontdb::Weight::BOLD {
                    got.map_or(0.0, embolden_for)
                } else {
                    0.0
                };
                println!("{label} 求 {weight:?} → 得 {got:?}（合成 {synth:.3} em）");
            }
        }
        println!("--- 本机所有中日韩简体字面 ---");
        for face in db.faces() {
            if face.families.iter().any(|(n, _)| n.ends_with("SC")) {
                println!(
                    "{:?} idx={} weight={:?} @ {:?}",
                    face.families.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
                    face.index,
                    face.weight,
                    face.source,
                );
            }
        }
    }
}
