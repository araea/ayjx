//! 原生绘制画布：不依赖浏览器的卡片出图底座。
//!
//! 文字用 `ab_glyph` 直接光栅化，几何图元用 SDF 逐像素判定覆盖度做抗锯齿。
//! 所有坐标都是「逻辑像素」，绘制时统一乘设备比例 `s`，出图分辨率随各插件的
//! `image_scale` 走，放大不糊。
//!
//! 与浏览器截图相比，这条路子换来三件事：出图不需要 Chrome、不受无头浏览器
//! 启动失败影响、单张卡片从数百毫秒降到数十毫秒。代价是版式要自己算，所以
//! 这里把度量（`text_w` / `wrap` / `ellipsis`）与绘制放在一起，排版代码只跟
//! 逻辑像素打交道。

// 绘图原语（x/y/w/h/r/ink…）参数个数是自然的，不做结构体打包
#![allow(clippy::too_many_arguments)]

use super::font::Face;
use ab_glyph::{Font, PxScale, ScaleFont, point};
use image::{Rgba, RgbaImage};

/// 带透明度的颜色。最终都往不透明底上 over 混合。
#[derive(Clone, Copy, Debug, PartialEq)]
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
        Ink {
            a: if a < 0.0 {
                0.0
            } else if a > 1.0 {
                1.0
            } else {
                a
            },
            ..self
        }
    }
    /// `#RRGGBB`
    pub fn hex(hex: &str) -> Self {
        let h = hex.trim_start_matches('#');
        let n = u32::from_str_radix(h, 16).unwrap_or(0x888888);
        Ink::rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
    }
    /// 两色之间线性插值（渐变条、底纹过渡）
    pub fn mix(self, other: Ink, t: f32) -> Ink {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
        Ink {
            r: lerp(self.r, other.r),
            g: lerp(self.g, other.g),
            b: lerp(self.b, other.b),
            a: self.a + (other.a - self.a) * t,
        }
    }
}

/// 行首不该出现的标点（避头点）。折行时若下一行以它们开头，
/// 就把它拉回上一行——中文排版里这一条最影响「读起来顺不顺」。
const NO_LINE_START: &[char] = &[
    '，', '。', '、', '；', '：', '！', '？', '）', '】', '」', '』', '》', '〉', '”', '’', '·',
    '…', '—', ',', '.', ';', ':', '!', '?', ')', ']', '}', '%',
];

/// 行尾不该出现的标点（避尾点）：开引号、开括号不落在行末。
const NO_LINE_END: &[char] = &['（', '【', '「', '『', '《', '〈', '“', '‘', '(', '[', '{'];

/// 可以在其后断行的 ASCII 字符：西文按词断，不再把单词劈成两半。
fn ascii_breakable(ch: char) -> bool {
    matches!(ch, ' ' | '-' | '/' | '_' | ',' | ';')
}

/// 该字符是否属于「可任意断行」的一类（CJK 与全角标点）
fn cjk_breakable(ch: char) -> bool {
    !ch.is_ascii_alphanumeric() && ch as u32 > 0x2000
}

/// 逻辑坐标画布：内部图像是逻辑尺寸 × `s`。
pub struct Canvas {
    pub img: RgbaImage,
    s: f32,
    /// 文字覆盖度的伽马修正。深底浅字线性混合会显得过细，
    /// `<1` 加粗笔画，`1.0` 关闭。见 [`Canvas::set_text_gamma`]。
    text_gamma: f32,
}

impl Canvas {
    pub fn new(w_logical: f32, h_logical: f32, s: f32) -> Self {
        let img = RgbaImage::new((w_logical * s).ceil() as u32, (h_logical * s).ceil() as u32);
        Canvas {
            img,
            s,
            text_gamma: 1.0,
        }
    }

    pub fn s(&self) -> f32 {
        self.s
    }

    /// 深底卡片建议 0.82 左右：浅色小字在暗底上按线性覆盖度合成会偏细偏灰，
    /// 抬一点覆盖度才接近浏览器里「字重正常」的观感。浅底卡片保持 1.0。
    pub fn set_text_gamma(&mut self, g: f32) {
        self.text_gamma = g.clamp(0.4, 2.0);
    }

    #[inline]
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
            if oa <= 0.0 {
                0
            } else {
                ((fg as f32 * a + bg as f32 * pa * (1.0 - a)) / oa) as u8
            }
        };
        *p = Rgba([
            mix(ink.r, p[0]),
            mix(ink.g, p[1]),
            mix(ink.b, p[2]),
            (oa * 255.0) as u8,
        ]);
    }

    /// 整幅填充（背景）。不透明色走直写，省掉逐像素混合。
    pub fn fill(&mut self, ink: Ink) {
        if ink.a >= 1.0 {
            let px = Rgba([ink.r, ink.g, ink.b, 255]);
            for p in self.img.pixels_mut() {
                *p = px;
            }
            return;
        }
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

    /// 竖向线性渐变填充矩形（页眉光晕、卡面过渡）
    pub fn vgrad(&mut self, x: f32, y: f32, w: f32, h: f32, top: Ink, bottom: Ink) {
        let rows = (h * self.s).ceil().max(1.0);
        for i in 0..rows as i32 {
            let t = i as f32 / rows;
            self.rect(
                x,
                y + i as f32 / self.s,
                w,
                1.0 / self.s,
                top.mix(bottom, t),
            );
        }
    }

    /// 横向线性渐变填充矩形（进度条）
    pub fn hgrad(&mut self, x: f32, y: f32, w: f32, h: f32, left: Ink, right: Ink) {
        let cols = (w * self.s).ceil().max(1.0);
        for i in 0..cols as i32 {
            let t = i as f32 / cols;
            self.rect(
                x + i as f32 / self.s,
                y,
                1.0 / self.s,
                h,
                left.mix(right, t),
            );
        }
    }

    /// 径向柔光：给深色卡面一点纵深。半径外按二次曲线衰减到全透明，
    /// 逐像素算，不需要真正的高斯模糊。
    pub fn glow(&mut self, cx: f32, cy: f32, radius: f32, ink: Ink) {
        let s = self.s;
        let (dcx, dcy, r) = (cx * s, cy * s, radius * s);
        let (w, h) = self.img.dimensions();
        let x0 = ((dcx - r).floor() as i32).max(0);
        let x1 = ((dcx + r).ceil() as i32).min(w as i32);
        let y0 = ((dcy - r).floor() as i32).max(0);
        let y1 = ((dcy + r).ceil() as i32).min(h as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                let d = (px as f32 - dcx).hypot(py as f32 - dcy) / r;
                if d >= 1.0 {
                    continue;
                }
                let t = 1.0 - d;
                self.blend(px, py, ink, t * t);
            }
        }
    }

    /// 圆角矩形 SDF：`d<=0` 在内部。`stroke` 为 None 时填充，否则描边。
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

    pub fn rrect_stroke(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        stroke_px: f32,
        ink: Ink,
    ) {
        self.rrect_sdf(x, y, w, h, r, ink, Some(stroke_px));
    }

    /// 圆角矩形的柔和外投影。
    ///
    /// 单遍 SDF：只在形状**外侧**着色，随距离二次衰减到 `spread` 处归零。
    /// 「只画外侧」这一条是刻意的——投影因此可以在卡面画好之后再补，
    /// 不会把已经画上去的内容盖掉（裁剪收边时要用到）。
    pub fn rrect_shadow(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, spread: f32, ink: Ink) {
        let s = self.s;
        let (hw, hh) = (w * s / 2.0, h * s / 2.0);
        let cx = (x * s) + hw;
        let cy = (y * s) + hh;
        let r = (r * s).min(hw).min(hh).max(0.0);
        let sp = (spread * s).max(1.0);

        let x0 = ((cx - hw - sp).floor() as i32).max(0);
        let x1 = ((cx + hw + sp).ceil() as i32).min(self.img.width() as i32);
        let y0 = ((cy - hh - sp).floor() as i32).max(0);
        let y1 = ((cy + hh + sp).ceil() as i32).min(self.img.height() as i32);

        for py in y0..y1 {
            for px in x0..x1 {
                let qx = (px as f32 - cx).abs() - (hw - r);
                let qy = (py as f32 - cy).abs() - (hh - r);
                let d = (qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0)) - r;
                if d <= 0.0 || d >= sp {
                    continue;
                }
                let t = 1.0 - d / sp;
                self.blend(px, py, ink, t * t);
            }
        }
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
        let period = if dash > 0.0 { (dash + gap) * s } else { 0.0 };
        for px in (ax.floor() as i32)..(bx.ceil() as i32) {
            if period > 0.0 && (px as f32 - ax) % period >= dash * s {
                continue;
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

    /// 竖虚线
    pub fn vdash(&mut self, x: f32, y0: f32, y1: f32, thick: f32, ink: Ink, dash: f32, gap: f32) {
        let s = self.s;
        let (ay, by) = ((y0.min(y1)) * s, (y0.max(y1)) * s);
        let period = if dash > 0.0 { (dash + gap) * s } else { 0.0 };
        let px0 = (x * s).floor() as i32;
        let px1 = ((x + thick) * s).ceil() as i32;
        for py in (ay.floor() as i32)..(by.ceil() as i32) {
            if period > 0.0 && (py as f32 - ay) % period >= dash * s {
                continue;
            }
            for px in px0..px1 {
                self.blend(px, py, ink, 1.0);
            }
        }
    }

    /// 虚线描一个矩形框
    pub fn dashed_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        thick: f32,
        ink: Ink,
        dash: f32,
        gap: f32,
    ) {
        self.hline(x, x + w, y, thick, ink, dash, gap);
        self.hline(x, x + w, y + h - thick, thick, ink, dash, gap);
        self.vdash(x, y, y + h, thick, ink, dash, gap);
        self.vdash(x + w - thick, y, y + h, thick, ink, dash, gap);
    }

    /// 质感点阵：在矩形区域内按 `step` 网格画小圆点。
    pub fn dot_grid(
        &mut self,
        x0: f32,
        y0: f32,
        w: f32,
        h: f32,
        step: f32,
        ox: f32,
        oy: f32,
        radius: f32,
        ink: Ink,
    ) {
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

    /// 方格底纹（坐标纸）：按 `step` 画横竖细线，`from` 起点对齐网格相位。
    pub fn line_grid(&mut self, x0: f32, y0: f32, w: f32, h: f32, step: f32, thick: f32, ink: Ink) {
        let mut gx = x0;
        while gx < x0 + w {
            self.vline(gx, y0, y0 + h, thick, ink);
            gx += step;
        }
        let mut gy = y0;
        while gy < y0 + h {
            self.hline(x0, x0 + w, gy, thick, ink, 0.0, 0.0);
            gy += step;
        }
    }

    // ================= 文字 =================

    /// 单个字符的步进宽度（逻辑像素）。缺字时按首选字体的 `.notdef` 步进，
    /// 保证「画不出来」与「量出来的宽度」一致，版式不会错位。
    fn advance(&self, face: &Face, ch: char, px: f32) -> f32 {
        let scale = PxScale::from(px * self.s);
        match face.glyph(ch) {
            Some((font, gid)) => font.as_scaled(scale).h_advance(gid) / self.s,
            None => {
                let font = face.primary();
                font.as_scaled(scale).h_advance(font.glyph_id(ch)) / self.s
            }
        }
    }

    /// 文字宽度（逻辑像素）
    pub fn text_w(&self, text: &str, face: &Face, px: f32, spacing: f32) -> f32 {
        let mut w = 0.0f32;
        let n = text.chars().count();
        for (i, ch) in text.chars().enumerate() {
            w += self.advance(face, ch, px);
            if i + 1 < n {
                w += spacing;
            }
        }
        w
    }

    /// 文字行高（逻辑像素）
    pub fn text_h(&self, face: &Face, px: f32) -> f32 {
        let sc = face.primary().as_scaled(PxScale::from(px * self.s));
        (sc.ascent() - sc.descent()) / self.s
    }

    /// 在基线处绘制一行文字，返回占用宽度。坐标为逻辑像素。
    pub fn text(
        &mut self,
        x: f32,
        baseline: f32,
        text: &str,
        face: &Face,
        px: f32,
        ink: Ink,
        spacing: f32,
    ) -> f32 {
        let s = self.s;
        let scale = PxScale::from(px * s);
        let gamma = self.text_gamma;
        let mut pen = x * s;
        for ch in text.chars() {
            // 缺字则整字跳过（不画豆腐块），但仍按度量步进
            if let Some((font, gid)) = face.glyph(ch) {
                let glyph = gid.with_scale_and_position(scale, point(pen, baseline * s));
                if let Some(og) = font.outline_glyph(glyph) {
                    let b = og.px_bounds();
                    let (bx, by) = (b.min.x as i32, b.min.y as i32);
                    og.draw(|gx, gy, cov| {
                        let cov = if gamma == 1.0 { cov } else { cov.powf(gamma) };
                        self.blend(bx + gx as i32, by + gy as i32, ink, cov);
                    });
                }
            }
            pen += self.advance(face, ch, px) * s + spacing * s;
        }
        (pen - x * s - if text.is_empty() { 0.0 } else { spacing * s }) / s
    }

    /// 水平垂直居中绘制
    pub fn text_center(
        &mut self,
        cx: f32,
        cy: f32,
        text: &str,
        face: &Face,
        px: f32,
        ink: Ink,
        spacing: f32,
    ) {
        let w = self.text_w(text, face, px, spacing);
        let sc = face.primary().as_scaled(PxScale::from(px * self.s));
        // top = cy - 行高/2；baseline = top + ascent（均换算回逻辑像素）
        let baseline = cy - (sc.ascent() - sc.descent()) / 2.0 / self.s + sc.ascent() / self.s;
        self.text(cx - w / 2.0, baseline, text, face, px, ink, spacing);
    }

    /// 按**字形实际墨迹**居中，而不是按步进宽度。
    ///
    /// 全角标点（尤其「？」「，」）的字身在字面里是偏侧的：按步进宽度居中，
    /// 看上去就会歪在格子一角。田字格里的占位问号、印章里的单字这类
    /// 「一个字要正对着一个框」的场合必须按墨迹居中。
    pub fn text_center_ink(
        &mut self,
        cx: f32,
        cy: f32,
        text: &str,
        face: &Face,
        px: f32,
        ink: Ink,
        spacing: f32,
    ) {
        // 量出墨迹包围盒后反推落笔点：让包围盒的中心正好落在 (cx, cy)
        match self.ink_bounds(text, face, px, spacing) {
            Some((x0, y0, x1, y1)) => {
                self.text(
                    cx - (x0 + x1) / 2.0,
                    cy - (y0 + y1) / 2.0,
                    text,
                    face,
                    px,
                    ink,
                    spacing,
                );
            }
            None => self.text_center(cx, cy, text, face, px, ink, spacing),
        }
    }

    /// 一段文字的墨迹包围盒，相对「落笔点 (0,0)、基线 y=0」，单位逻辑像素。
    /// 全是空白或缺字时返回 None。
    pub fn ink_bounds(
        &self,
        text: &str,
        face: &Face,
        px: f32,
        spacing: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        let s = self.s;
        let scale = PxScale::from(px * s);
        let mut pen = 0.0f32;
        let mut acc: Option<(f32, f32, f32, f32)> = None;
        for ch in text.chars() {
            if let Some((font, gid)) = face.glyph(ch)
                && let Some(og) =
                    font.outline_glyph(gid.with_scale_and_position(scale, point(pen, 0.0)))
            {
                let b = og.px_bounds();
                acc = Some(match acc {
                    None => (b.min.x, b.min.y, b.max.x, b.max.y),
                    Some((x0, y0, x1, y1)) => (
                        x0.min(b.min.x),
                        y0.min(b.min.y),
                        x1.max(b.max.x),
                        y1.max(b.max.y),
                    ),
                });
            }
            pen += self.advance(face, ch, px) * s + spacing * s;
        }
        acc.map(|(x0, y0, x1, y1)| (x0 / s, y0 / s, x1 / s, y1 / s))
    }

    /// 右对齐绘制
    pub fn text_right(
        &mut self,
        rx: f32,
        baseline: f32,
        text: &str,
        face: &Face,
        px: f32,
        ink: Ink,
        spacing: f32,
    ) {
        let w = self.text_w(text, face, px, spacing);
        self.text(rx - w, baseline, text, face, px, ink, spacing);
    }

    /// 超宽截断加省略号，返回截断后的文本
    pub fn ellipsis(&self, text: &str, face: &Face, px: f32, spacing: f32, max_w: f32) -> String {
        if self.text_w(text, face, px, spacing) <= max_w {
            return text.to_string();
        }
        let mut s = text.to_string();
        while !s.is_empty() {
            s.pop();
            let cand = format!("{s}…");
            if self.text_w(&cand, face, px, spacing) <= max_w {
                return cand;
            }
        }
        "…".to_string()
    }

    /// 按宽度折行，最多 `max_lines` 行；放不下时末行加省略号。
    ///
    /// 三条排版规则，都是为了「读起来不别扭」：
    ///   - 西文按词断（只在空格、连字符等处换行），不把单词劈成两半；
    ///   - 避头点：下一行不以逗号句号右括号开头；
    ///   - 避尾点：本行不以左括号左引号结尾。
    pub fn wrap(
        &self,
        text: &str,
        face: &Face,
        px: f32,
        spacing: f32,
        max_w: f32,
        max_lines: usize,
    ) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for para in text.split('\n') {
            if lines.len() >= max_lines {
                break;
            }
            self.wrap_para(para, face, px, spacing, max_w, max_lines, &mut lines);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    fn wrap_para(
        &self,
        text: &str,
        face: &Face,
        px: f32,
        spacing: f32,
        max_w: f32,
        max_lines: usize,
        lines: &mut Vec<String>,
    ) {
        let chars: Vec<char> = text.chars().collect();
        let mut cur: Vec<char> = Vec::new();
        let mut w = 0.0f32;
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            let cw = self.advance(face, ch, px) + if cur.is_empty() { 0.0 } else { spacing };
            if !cur.is_empty() && w + cw > max_w {
                // 已经是最后一行：截断加省略号，整段结束
                if lines.len() + 1 >= max_lines {
                    let mut s: String = cur.iter().collect();
                    while !s.is_empty() && self.text_w(&format!("{s}…"), face, px, spacing) > max_w
                    {
                        s.pop();
                    }
                    lines.push(format!("{s}…"));
                    return;
                }
                let brk = self.break_point(&cur, ch);
                let carry: Vec<char> = cur.split_off(brk);
                lines.push(cur.iter().collect());
                cur = carry;
                w = self.text_w(&cur.iter().collect::<String>(), face, px, spacing);
                continue; // 不推进 i，当前字符重新参与下一行的排布
            }
            cur.push(ch);
            w += cw;
            i += 1;
        }
        if !cur.is_empty() || lines.is_empty() {
            lines.push(cur.iter().collect());
        }
    }

    /// 在已排满的一行里找断点：返回应当移到下一行的起始下标。
    /// 找不到合适位置时就在末尾硬断（返回 `cur.len()`）。
    fn break_point(&self, cur: &[char], next: char) -> usize {
        let n = cur.len();
        // 避头点：下一行不能以这些标点开头，把行末字符一起带下去
        if NO_LINE_START.contains(&next) && n > 1 {
            return n - 1;
        }
        // 避尾点：本行不能以开括号/开引号结尾
        if let Some(&last) = cur.last()
            && NO_LINE_END.contains(&last)
            && n > 1
        {
            return n - 1;
        }
        // 西文：往回找最近的可断处，回溯不超过 16 个字符，免得留下大片空白
        if next.is_ascii_alphanumeric() {
            let floor = n.saturating_sub(16);
            for k in (floor + 1..n).rev() {
                if ascii_breakable(cur[k - 1]) {
                    return k;
                }
                if cjk_breakable(cur[k - 1]) {
                    return k;
                }
            }
        }
        n
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

    /// 编码为 PNG 的 base64（消息里以 `base64://` 直接发送）
    pub fn into_png_base64(self, h_logical: f32) -> Option<String> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let img = self.crop(h_logical);
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        Some(STANDARD.encode(png))
    }
}
