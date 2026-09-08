//! 把词意的每一次回应排版成一张卡片图（原生绘制，不走浏览器）。
//!
//! 设计取向：**宣纸 + 朱砂**。词意猜的是汉字与词义，所以整张卡走纸墨一路——
//! 暖白纸底带极淡的纤维颗粒，古籍式的内外双框，正文汉字一律用宋体，
//! 数字与元信息用无衬线压在灰阶里，只有「朱砂红」一种强调色，
//! 用在印章、名次、新提示这几处，其余全部让给内容。
//!
//! 几个刻意为之的细节：
//!   - **田字格**：未揭晓时是两个虚线田字格里的「？」，猜中后同一位置换成实线格
//!     配答案二字——同一个位置、同一种格子，把「悬念 → 揭晓」画成一个动作；
//!   - **邻词做成留白牌**：`？佩` 不是一串符号，而是一枚虚线小牌，
//!     未知的那个字用浅色「？」占位，一眼看得出「缺哪个字」；
//!   - **接近度按对数刻度画条**：名次 1 与 10 的差距远大于 900 与 1000，
//!     线性条会把前排全挤成满格，取 `1 - ln(rank)/ln(pool)` 才能把前排拉开；
//!   - **五档距离命名**（咫尺/相邻/相近/相关/天涯）：让「#137」这种抽象数字
//!     有一个可直接读出来的语感，颜色同步从朱砂过渡到远山蓝。
//!
//! 绘制基元见 `painter.rs`：文字由 ab_glyph 直接光栅化，几何用 SDF 逐像素判定。
//! 版心 668 逻辑 px，配合 `image_scale`（默认 3 倍）出图约 2160px 宽。

use super::view::{COMMAND_ROWS, Board, HintRow, RankBoard, RankItem, Reply, Win};
use crate::plugins::ciyi::painter::{Canvas, Fonts, Ink};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{FixedOffset, Utc};
use image::RgbaImage;

// ================= 版式常量（逻辑像素） =================

/// 视口宽 = 版心 720
const VIEW_W: f32 = 720.0;
/// 四周留白
const PAD: f32 = 26.0;
/// 卡片宽
const CARD_W: f32 = VIEW_W - PAD * 2.0;
/// 内容左右边界（卡片内边距 44）
const CX0: f32 = PAD + 44.0;
const CX1: f32 = PAD + CARD_W - 44.0;
/// 内容宽度
const CW: f32 = CX1 - CX0;

// ================= 色板 =================

const SHELL_BG: Ink = Ink::rgb(220, 210, 190); // #DCD2BE
const PAPER: Ink = Ink::rgb(250, 246, 236); // #FAF6EC
const INK: Ink = Ink::rgb(36, 31, 27); // #241F1B
const INK2: Ink = Ink::rgb(75, 67, 58); // #4B433A
const INK3: Ink = Ink::rgb(139, 127, 112); // #8B7F70
const RED: Ink = Ink::rgb(176, 52, 42); // #B0342A
const CREAM: Ink = Ink::rgb(251, 247, 239); // #FBF7EF
const SEPIA: Ink = Ink::rgb(120, 95, 65); // 线与底纹
const FOOT_C: Ink = Ink::rgb(154, 142, 126); // #9A8E7E
const NB_C: Ink = Ink::rgb(126, 114, 100); // #7E7264 邻词牌
const NOTE_C: Ink = Ink::rgb(109, 74, 64); // #6D4A40 提示条

// ================= 通用小件 =================

/// 出图时刻（北京时间），压在页眉右上角
fn stamp() -> String {
    let tz = FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移");
    Utc::now().with_timezone(&tz).format("%m-%d %H:%M").to_string()
}

/// 千分位：词池动辄四五位数，不分节读不出量级
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 名次对应的「距离感」：一个能读出来的词 + 一种颜色。
///
/// 分档参照词意的实际手感：前十基本咬住答案，五十以内已在同一语义簇，
/// 两百以内还算相近，千以内只是沾亲带故，再往后就是天涯了。
fn tier(rank: usize) -> (&'static str, &'static str) {
    match rank {
        0..=10 => ("咫尺", "#B0342A"),
        11..=50 => ("相邻", "#C0662A"),
        51..=200 => ("相近", "#A08420"),
        201..=1000 => ("相关", "#4B7A62"),
        _ => ("天涯", "#5D6C85"),
    }
}

/// 接近度条的长度。
///
/// 取对数刻度：名次每翻一倍，条长退一档固定距离。线性刻度下
/// 名次 1 和名次 50 在一个万词的词池里几乎一样长，那条就白画了。
fn heat(rank: usize, pool: usize) -> f64 {
    let rank = rank.max(1) as f64;
    let pool = pool.max(rank as usize + 1) as f64;
    (1.0 - rank.ln() / pool.ln()).clamp(0.04, 1.0)
}

/// 垂直居中的文字基线：CJK 字面约占 0.88em，视觉重心略高于几何中心
fn mid_baseline(cy: f32, px: f32) -> f32 {
    cy + 0.38 * px
}

/// 白与色之间的插值（接近度条 / 底纹渐变）
fn mix(a: Ink, b: Ink, t: f32) -> Ink {
    let t = t.clamp(0.0, 1.0);
    Ink {
        r: (a.r as f32 + (b.r as f32 - a.r as f32) * t) as u8,
        g: (a.g as f32 + (b.g as f32 - a.g as f32) * t) as u8,
        b: (a.b as f32 + (b.b as f32 - a.b as f32) * t) as u8,
        a: a.a + (b.a - a.a) * t,
    }
}

/// 虚线描一个矩形框（圆角近似为直角，框小看不出来）
fn dashed_rect(c: &mut Canvas, x: f32, y: f32, w: f32, h: f32, ink: Ink) {
    c.hline(x, x + w, y, 1.0, ink, 4.0, 4.0);
    c.hline(x, x + w, y + h, 1.0, ink, 4.0, 4.0);
    c.vdash(x, y, y + h, 1.0, ink, 4.0, 4.0);
    c.vdash(x + w, y, y + h, 1.0, ink, 4.0, 4.0);
}

/// 点状分隔线（行与行之间）
fn dotted_sep(c: &mut Canvas, y: f32) {
    c.hline(CX0 + 12.0, CX1 - 12.0, y, 1.0, SEPIA.with_a(0.20), 1.0, 3.0);
}

/// 渐变进度条：左端混白提亮，右端落到本色
fn gradient_bar(c: &mut Canvas, x: f32, y: f32, w: f32, h: f32, color: Ink) {
    let cols = w.ceil() as i32;
    for i in 0..cols {
        let t = i as f32 / w.max(1.0);
        let ink = mix(Ink::rgb(255, 255, 255).with_a(0.45), color, t);
        c.rect(x + i as f32, y, 1.0, h, ink);
    }
}

// ================= 公共部件 =================

/// 田字格。`word` 为空则画成虚线格里的「？」，即尚未揭晓的答案。
/// `large` 对应揭晓卡的放大版（104px 格、60px 字、重描边）。
fn draw_cells(c: &mut Canvas, f: &Fonts, x: f32, y: f32, word: &str, large: bool) {
    let (cell, font_px, blank_px, border_a) = if large {
        (104.0, 60.0, 0.0, 0.5)
    } else {
        (50.0, 26.0, 21.0, 0.30)
    };
    let chars: Vec<char> = word.chars().collect();
    let n = if chars.is_empty() { 2 } else { chars.len() };
    let gap = 10.0;
    let mut cx = x;
    for i in 0..n {
        let ch = chars.get(i);
        // 底：半透明白
        c.rrect_fill(cx, y, cell, cell, 4.0, Ink::rgb(255, 255, 255).with_a(0.5));
        // 边框
        match ch {
            Some(_) => c.rrect_stroke(cx, y, cell, cell, 4.0, 2.0, RED.with_a(border_a)),
            None => {
                let ink = RED.with_a(border_a);
                let dash = 5.0;
                let g = 4.0;
                c.hline(cx, cx + cell, y, 2.0, ink, dash, g);
                c.hline(cx, cx + cell, y + cell - 2.0, 2.0, ink, dash, g);
                c.vdash(cx, y, y + cell, 2.0, ink, dash, g);
                c.vdash(cx + cell - 2.0, y, y + cell, 2.0, ink, dash, g);
            }
        }
        // 田字虚线十字
        let inner = cell * 0.09;
        let cy = y + cell / 2.0;
        let xc = cx + cell / 2.0;
        c.hline(cx + inner, cx + cell - inner, cy, 1.0, RED.with_a(border_a * 0.87), 4.0, 4.0);
        c.vdash(xc, y + inner, y + cell - inner, 1.0, RED.with_a(border_a * 0.87), 4.0, 4.0);
        // 字
        match ch {
            Some(&ch) => {
                c.text_center(xc, cy, &ch.to_string(), &f.serif_b, font_px, INK, 0.0);
            }
            None => {
                // 全角「？」的字形偏左，往右挪四分之一个字身压回中线
                c.text_center(xc + 0.25 * blank_px, cy, "？", &f.serif_b, blank_px, RED.with_a(0.42), 0.0);
            }
        }
        cx += cell + gap;
    }
}

fn cells_width(n: usize, cell: f32) -> f32 {
    cell * n as f32 + 10.0 * (n as f32 - 1.0).max(0.0)
}

/// 右上角对齐的两格虚线田字格（盘面 / 玩法页眉配图）
fn draw_aside_cells(c: &mut Canvas, f: &Fonts, right: f32, bottom: f32) {
    let cell = 50.0;
    let w = cells_width(2, cell);
    draw_cells(c, f, right - w, bottom - cell, "", false);
}

/// 邻词牌：缺的那个字用浅色「？」占位，缺口一眼可见。
/// `leading` 为真表示「？」在前（`？佩`），否则在后（`冥？`）。
fn draw_nb(c: &mut Canvas, f: &Fonts, x: f32, cy: f32, text: &str, leading: bool) -> f32 {
    let px = 19.0;
    let spacing = 0.1 * px;
    let w = c.text_w(text, &f.serif_b, px, spacing) + 20.0 + 2.0;
    let h = 33.0;
    c.rrect_fill(x, cy - h / 2.0, w, h, 6.0, SEPIA.with_a(0.055));
    dashed_rect(c, x, cy - h / 2.0, w, h, SEPIA.with_a(0.24));
    let baseline = mid_baseline(cy, px);
    let chars: Vec<char> = text.chars().collect();
    let mut tx = x + 11.0;
    for (i, ch) in chars.iter().enumerate() {
        let is_q = (leading && i == 0) || (!leading && i + 1 == chars.len());
        let ink = if is_q { SEPIA.with_a(0.42) } else { NB_C };
        tx += c.text(tx, baseline, &ch.to_string(), &f.serif_b, px, ink, spacing) + spacing;
    }
    w
}

/// 一条提示：名次 + 距离档 | 邻词牌 · 猜过的词 · 邻词牌 + 接近度条 | 「新」印
/// 返回新的 y。
fn draw_hint_row(c: &mut Canvas, f: &Fonts, y: f32, row: &HintRow, pool: usize, last: bool) -> f32 {
    const ROW_H: f32 = 77.0;
    const TRI_H: f32 = 37.0;
    let (label, color) = tier(row.rank);
    let ink = Ink::hex(color);

    if row.fresh {
        c.rrect_fill(CX0, y, CW, ROW_H, 9.0, RED.with_a(0.055));
        c.rrect_fill(CX0, y + 9.0, 3.0, ROW_H - 18.0, 1.5, RED);
    }

    // 名次 + 距离档（右对齐）
    let rk_right = CX0 + 84.0;
    let num = row.rank.to_string();
    let hash_w = c.text_w("#", &f.sans_b, 16.0, 0.0);
    let num_w = c.text_w(&num, &f.sans_b, 25.0, 0.0);
    let base1 = y + 14.0 + 19.5;
    let sx = rk_right - hash_w - 1.0 - num_w;
    c.text(sx, base1, "#", &f.sans_b, 16.0, ink.with_a(0.5), 0.0);
    c.text(sx + hash_w + 1.0, base1, &num, &f.sans_b, 25.0, ink, 0.0);
    c.text_right(rk_right, base1 + 7.0 + 9.5, label, &f.serif_b, 12.5, ink.with_a(0.8), 2.5);

    // 中列：邻词牌 + 词 + 邻词牌，垂直居中在 TRI_H 里
    let mx = CX0 + 100.0;
    let mw = CW - 84.0 - 32.0 - 32.0;
    let tri_y = y + 14.0;
    let cy = tri_y + TRI_H / 2.0;
    let mut tx = mx;
    tx += draw_nb(c, f, tx, cy, &format!("？{}", row.prev), true) + 18.0;
    let wd_base = mid_baseline(cy, 31.0);
    c.text(tx, wd_base, &row.word, &f.serif_b, 31.0, INK, 0.12 * 31.0);
    tx += c.text_w(&row.word, &f.serif_b, 31.0, 0.12 * 31.0) + 18.0;
    draw_nb(c, f, tx, cy, &format!("{}？", row.next), false);

    // 接近度条（对数刻度）
    let bar_y = tri_y + TRI_H + 11.0;
    c.rrect_fill(mx, bar_y, mw, 5.0, 2.5, SEPIA.with_a(0.10));
    let bw = (mw as f64 * heat(row.rank, pool)).max(5.0) as f32;
    gradient_bar(c, mx, bar_y, bw, 5.0, ink);

    // 「新」印
    if row.fresh {
        draw_flag(c, f, CX1 - 16.0, y + ROW_H / 2.0);
    }

    if !last {
        dotted_sep(c, y + ROW_H);
    }
    y + ROW_H
}

/// 「新」小旗：旋转 -6° 的朱砂方印
fn draw_flag(c: &mut Canvas, f: &Fonts, cx: f32, cy: f32) {
    let mut layer = Canvas::new(30.0, 30.0, c.s());
    layer.rrect_fill(2.0, 2.0, 26.0, 26.0, 5.0, RED);
    layer.text_center(15.0, 15.0, "新", &f.serif_b, 14.0, CREAM, 0.0);
    c.blit_rotated(&layer.img, cx, cy, -6.0);
}

/// 「注」提示条
fn draw_note(c: &mut Canvas, f: &Fonts, y: f32, text: &str) -> f32 {
    let y = y + 18.0;
    let h = 48.0;
    c.rrect_fill(CX0, y, CW, h, 8.0, RED.with_a(0.06));
    c.rrect_fill(CX0, y + 8.0, 3.0, h - 16.0, 1.5, RED);
    c.rrect_fill(CX0 + 16.0, y + 12.0, 24.0, 24.0, 4.0, RED.with_a(0.14));
    c.text_center(CX0 + 28.0, y + 24.0, "注", &f.serif_b, 14.0, RED, 0.0);
    c.text(CX0 + 51.0, mid_baseline(y + 24.0, 16.0), text, &f.sans, 16.0, NOTE_C, 0.0);
    y + h
}

/// 空盘占位框
fn draw_empty(c: &mut Canvas, f: &Fonts, y: f32, text: &str) -> f32 {
    let y = y + 22.0;
    let h = 34.0 * 2.0 + 24.0;
    dashed_rect(c, CX0, y, CW, h, SEPIA.with_a(0.20));
    c.text_center(CX0 + CW / 2.0, y + h / 2.0, text, &f.serif_b, 18.0, INK3, 0.06 * 18.0);
    y + h + 6.0
}

/// 数字卡一排：`items` 为 (值, 标签)，固定三格
fn draw_stats(c: &mut Canvas, f: &Fonts, y: f32, items: &[(String, &str)]) -> f32 {
    let gap = 13.0;
    let tw = (CW - gap * 2.0) / 3.0;
    let h = 81.0;
    for (i, (val, label)) in items.iter().enumerate() {
        let x = CX0 + (tw + gap) * i as f32;
        c.rrect_fill(x, y, tw, h, 10.0, Ink::rgb(255, 255, 255).with_a(0.55));
        c.rrect_stroke(x, y, tw, h, 10.0, 1.0, SEPIA.with_a(0.20));
        c.text_center(x + tw / 2.0, y + 29.0, val, &f.sans_b, 26.0, INK, 0.0);
        c.text_center(x + tw / 2.0, y + 56.0, label, &f.sans, 12.0, INK3, 0.16 * 12.0);
    }
    y + h
}

// ================= 卡片外壳 =================

/// 背景、纸纹、内外双框、页眉、标题区、朱砂分隔线。返回正文起始 y。
fn shell_head(c: &mut Canvas, f: &Fonts, title: &str, sub: &str, aside: bool) -> f32 {
    let h = c.img.height() as f32 / c.s();
    // 托纸
    c.fill(SHELL_BG);
    c.dot_grid(0.0, 0.0, VIEW_W, h, 7.0, 0.0, 0.0, 1.0, Ink::rgb(255, 255, 255).with_a(0.42));
    // 宣纸卡：暖白底 + 两层纤维颗粒
    c.rrect_fill(PAD, PAD, CARD_W, h - PAD * 2.0, 5.0, PAPER);
    let paper = |cc: &mut Canvas| {
        cc.dot_grid(PAD, PAD, CARD_W, h - PAD * 2.0, 13.0, 0.0, 0.0, 1.0, Ink::rgb(120, 90, 60).with_a(0.045));
        cc.dot_grid(PAD, PAD, CARD_W, h - PAD * 2.0, 23.0, 8.0, 11.0, 1.0, Ink::rgb(120, 90, 60).with_a(0.032));
    };
    paper(c);
    // 古籍式内框：离纸边 11px 的一道细线
    c.rrect_stroke(PAD + 11.0, PAD + 11.0, CARD_W - 22.0, h - PAD * 2.0 - 22.0, 2.0, 1.0, SEPIA.with_a(0.16));

    // —— 页眉 ——
    let head_y = PAD + 42.0;
    let head_cy = head_y + 21.0;
    // 印章：朱砂底 + 旋转 -3° 的「词」
    let mut seal = Canvas::new(46.0, 46.0, c.s());
    seal.rrect_fill(2.0, 2.0, 42.0, 42.0, 5.0, RED);
    seal.text_center(23.0, 23.0, "词", &f.serif_b, 25.0, CREAM, 0.0);
    c.blit_rotated(&seal.img, CX0 + 21.0, head_cy, -3.0);
    // 品名
    let bx = CX0 + 42.0 + 13.0;
    c.text(bx, head_y + 20.0, "词意", &f.serif_b, 20.0, INK, 0.24 * 20.0);
    c.text(bx, head_y + 24.0 + 4.0 + 12.0, "每日一词 · 语义寻踪", &f.sans, 12.0, INK3, 0.2 * 12.0);
    // 时刻
    c.text_right(CX1, mid_baseline(head_cy, 13.0), &stamp(), &f.sans, 13.0, INK3, 0.0);

    // —— 标题区 ——
    let lede_y = head_y + 42.0 + 22.0;
    c.text(CX0, lede_y + 37.0, title, &f.serif_b, 38.0, INK, 0.06 * 38.0);
    let mut bottom = lede_y + 46.0;
    if !sub.is_empty() {
        c.text(CX0, bottom + 11.0 + 17.0, sub, &f.sans, 14.5, INK3, 0.0);
        bottom += 11.0 + 23.0;
    }
    if aside {
        draw_aside_cells(c, f, CX1, bottom);
    }

    // —— 朱砂起首的分隔线 ——
    let ry = bottom + 22.0;
    c.hline(CX0, CX1, ry, 2.0, SEPIA.with_a(0.22), 0.0, 0.0);
    c.hline(CX0, CX0 + 54.0, ry, 2.0, RED, 0.0, 0.0);
    ry + 2.0 + 6.0
}

/// 页脚 + 纸卡收边。返回整卡的内容底界（含内外留白），用于裁剪。
///
/// 纸卡与内框在 `shell_head` 里是按预分配画布高度画的，远超实际内容；
/// 若直接裁剪，纸卡的圆角底边会落在裁剪线之外——图片底缘是纸色平切，
/// 看起来就像「下端被截掉」。这里在内容底界处用壳色擦出纸卡底边，
/// 再补画圆角收边、内框底线与壳面纹理，让卡片在裁剪线内完整闭合。
fn shell_foot(c: &mut Canvas, f: &Fonts, y: f32, foot: &str) -> f32 {
    let fy = y + 26.0;
    c.hline(CX0, CX1, fy, 1.0, SEPIA.with_a(0.16), 0.0, 0.0);
    c.text(CX0, fy + 16.0 + 10.0, foot, &f.sans, 12.5, FOOT_C, 0.0);
    // 三枚圆点收尾
    let dcy = fy + 16.0 + 5.0;
    c.circle_fill(CX1 - 3.0, dcy, 2.5, RED.with_a(0.55));
    c.circle_fill(CX1 - 13.0, dcy, 2.5, SEPIA.with_a(0.28));
    c.circle_fill(CX1 - 23.0, dcy, 2.5, SEPIA.with_a(0.16));

    // 纸卡底边 = 页脚文字底 + 30px 纸内留白；最终裁剪线再留一圈壳边
    let card_bottom = fy + 16.0 + 15.0 + 30.0;
    let alloc_h = c.img.height() as f32 / c.s();
    if card_bottom + PAD <= alloc_h {
        // 1) 擦掉纸卡底边以下的多画部分，还原为壳底色
        c.rect(0.0, card_bottom, VIEW_W, alloc_h - card_bottom, SHELL_BG);
        // 2) 重画该区域的壳面纹理（对齐全局 7px 网格相位，避免接缝错位）
        let oy = (7.0 - card_bottom % 7.0) % 7.0;
        c.dot_grid(0.0, card_bottom, VIEW_W, alloc_h - card_bottom, 7.0, 0.0, oy, 1.0, Ink::rgb(255, 255, 255).with_a(0.42));
        // 3) 纸卡底部圆角收边：同色条带叠在纸面上，只让下缘出现圆角
        c.rrect_fill(PAD, card_bottom - 12.0, CARD_W, 12.0, 5.0, PAPER);
        // 4) 内框重描到底：顶边重复描一遍同色细线，视觉无差
        c.rrect_stroke(PAD + 11.0, PAD + 11.0, CARD_W - 22.0, card_bottom - PAD - 22.0, 2.0, 1.0, SEPIA.with_a(0.16));
    }
    card_bottom + PAD
}

// ================= 四种卡片 =================

/// 盘面卡：一局进行中的全部有效提示，按名次从近到远排
pub fn board_card(c: &mut Canvas, f: &Fonts, board: &Board) -> f32 {
    let sub = format!(
        "已猜 {} 次 · 命中 {} 词 · 词池 {}",
        group(board.guesses),
        group(board.hits),
        group(board.pool)
    );
    let mut y = shell_head(c, f, "今日词意", &sub, true);

    if let Some(notice) = &board.notice {
        y = draw_note(c, f, y, notice);
    }

    if board.rows.is_empty() {
        y = draw_empty(c, f, y, "还没有命中排名的词，换个方向再试一个");
    } else {
        for (i, row) in board.rows.iter().enumerate() {
            y = draw_hint_row(c, f, y, row, board.pool, i + 1 == board.rows.len());
        }
    }

    y += 24.0;
    let best = board
        .rows
        .first()
        .map(|r| format!("#{}", r.rank))
        .unwrap_or_else(|| "—".into());
    y = draw_stats(
        c,
        f,
        y,
        &[
            (group(board.guesses), "已猜次数"),
            (group(board.hits), "命中排名"),
            (best, "最近名次"),
        ],
    );

    let foot = match board.hidden {
        0 => "左邻更近 · 右邻更远 · ？为待猜的字".to_string(),
        n => format!("另有 {n} 条更远的记录未列出 · 左邻更近 · 右邻更远"),
    };
    shell_foot(c, f, y, &foot)
}

/// 揭晓卡：田字格从虚线换成实线，答案落格，右侧盖一枚「猜中」圆印
pub fn win_card(c: &mut Canvas, f: &Fonts, win: &Win) -> f32 {
    let mut y = shell_head(c, f, "猜对了", &format!("今日答案「{}」已揭晓", win.answer), false);

    // 揭晓区：田字格 + 圆印
    y += 30.0;
    let cell = 104.0;
    let cells_w = cells_width(2, cell);
    let mark_d = 104.0;
    let total = cells_w + 34.0 + mark_d;
    let x = CX0 + (CW - total) / 2.0;
    draw_cells(c, f, x, y, &win.answer, true);

    let mut mark = Canvas::new(mark_d + 8.0, mark_d + 8.0, c.s());
    let m = mark_d / 2.0 + 4.0;
    mark.circle_stroke(m, m, mark_d / 2.0 - 1.5, 3.0, RED.with_a(0.7));
    mark.circle_stroke(m, m, mark_d / 2.0 - 4.0, 1.0, RED.with_a(0.16));
    mark.text_center(m, m - 15.6, "猜", &f.serif_b, 29.0, RED.with_a(0.82), 0.08 * 29.0);
    mark.text_center(m, m + 15.6, "中", &f.serif_b, 29.0, RED.with_a(0.82), 0.08 * 29.0);
    c.blit_rotated(&mark.img, x + cells_w + 34.0 + mark_d / 2.0, y + cell / 2.0, -9.0);
    y += cell + 30.0 + 6.0;

    // 数字卡：第三格只有当局历时超过一分钟才出现
    y += 24.0;
    let mut stats = vec![
        (group(win.guesses), "猜测次数"),
        (group(win.hits), "命中排名"),
    ];
    if let Some(elapsed) = win.elapsed() {
        stats.push((elapsed, "本局历时"));
    }
    y = draw_stats(c, f, y, &stats);

    // 猜中者独占一行：昵称长短不受控，塞进数字卡里六个字就到头了
    y += 13.0;
    y = draw_winner(c, f, y, &win.winner);

    shell_foot(c, f, y, "明日零点换新词 · 猜中次数已计入排行")
}

/// 猜中者通栏：昵称最多两行，超长断行后省略
fn draw_winner(c: &mut Canvas, f: &Fonts, y: f32, winner: &str) -> f32 {
    const PAD_Y: f32 = 15.0;
    const PAD_X: f32 = 20.0;
    const LH: f32 = 32.0;
    let span = "猜中者";
    let span_w = c.text_w(span, &f.sans, 12.0, 0.16 * 12.0);
    let name_x = CX0 + PAD_X + span_w + 14.0;
    let name_max = CX1 - PAD_X - name_x;
    let lines = c.wrap(winner, &f.serif_b, 22.0, 0.04 * 22.0, name_max, 2);
    let h = PAD_Y + lines.len() as f32 * LH + PAD_Y;

    c.rrect_fill(CX0, y, CW, h, 10.0, RED.with_a(0.055));
    c.rrect_stroke(CX0, y, CW, h, 10.0, 1.0, RED.with_a(0.16));
    c.text(CX0 + PAD_X, y + PAD_Y + 19.0, span, &f.sans, 12.0, INK3, 0.16 * 12.0);
    for (i, line) in lines.iter().enumerate() {
        c.text(name_x, y + PAD_Y + 19.0 + i as f32 * LH, line, &f.serif_b, 22.0, INK, 0.04 * 22.0);
    }
    y + h
}

/// 排行榜卡：前三名实心朱砂号牌，其余描边；条长按最高分归一化
pub fn rank_card(c: &mut Canvas, f: &Fonts, rank: &RankBoard) -> f32 {
    let mut y = shell_head(c, f, &rank.title, &rank.subtitle, false);

    if rank.items.is_empty() {
        y = draw_empty(c, f, y, "当前还没有人猜对过，第一个名字留给你");
        return shell_foot(c, f, y, "猜中一次即入榜");
    }

    let top = rank.items.iter().map(|i| i.score).max().unwrap_or(1).max(1) as f32;
    for (idx, item) in rank.items.iter().enumerate() {
        y = draw_rank_row(c, f, y, idx + 1, item, top, idx + 1 == rank.items.len());
    }

    // 榜单只有一种量纲（猜中次数），条长按榜首归一化即可，不必再分色
    y += 24.0;
    let total: i64 = rank.items.iter().map(|i| i.score).sum();
    y = draw_stats(
        c,
        f,
        y,
        &[
            (group(rank.items.len()), "上榜人数"),
            (group(total as usize), "合计猜中"),
            (group(top as usize), "榜首战绩"),
        ],
    );

    shell_foot(c, f, y, "每日一词 · 每人每日至多一胜")
}

fn draw_rank_row(c: &mut Canvas, f: &Fonts, y: f32, place: usize, item: &RankItem, top: f32, last: bool) -> f32 {
    const ROW_H: f32 = 72.0;
    // 号牌
    let lx = CX0 + 12.0;
    let ly = y + (ROW_H - 38.0) / 2.0;
    if place <= 3 {
        c.rrect_fill(lx, ly, 38.0, 38.0, 9.0, RED);
        c.text_center(lx + 19.0, ly + 19.0, &place.to_string(), &f.sans_b, 17.0, CREAM, 0.0);
    } else {
        c.rrect_fill(lx, ly, 38.0, 38.0, 9.0, RED.with_a(0.09));
        c.rrect_stroke(lx, ly, 38.0, 38.0, 9.0, 1.0, RED.with_a(0.22));
        c.text_center(lx + 19.0, ly + 19.0, &place.to_string(), &f.sans_b, 17.0, RED, 0.0);
    }

    // 名字 + 条
    let name_x = lx + 38.0 + 16.0;
    let name_max = CX1 - 78.0 - 16.0 - name_x;
    let name = c.ellipsis(&item.name, &f.serif_b, 22.0, 0.04 * 22.0, name_max);
    c.text(name_x, y + 15.0 + 19.0, &name, &f.serif_b, 22.0, INK, 0.04 * 22.0);
    let bar_y = y + 15.0 + 26.0 + 11.0;
    c.rrect_fill(name_x, bar_y, name_max, 5.0, 2.5, SEPIA.with_a(0.10));
    let bw = (name_max * item.score as f32 / top).max(5.0);
    c.rrect_fill(name_x, bar_y, bw, 5.0, 2.5, RED);

    // 分数
    let score = item.score.to_string();
    let unit_w = c.text_w("胜", &f.serif_b, 14.0, 0.0);
    let score_w = c.text_w(&score, &f.sans_b, 26.0, 0.0);
    c.text(CX1 - unit_w, y + 15.0 + 19.0, "胜", &f.serif_b, 14.0, INK3, 0.0);
    c.text(CX1 - unit_w - 4.0 - score_w, y + 15.0 + 19.0, &score, &f.sans_b, 26.0, INK, 0.0);

    if !last {
        dotted_sep(c, y + ROW_H);
    }
    y + ROW_H
}

/// 指令卡：左栏指令与别名，右栏一句话说明。
///
/// 指令一律带上本机真实的前缀，图里看到什么就照抄什么，不用再猜要不要加斜杠。
pub fn help_card(c: &mut Canvas, f: &Fonts, prefix: &str) -> f32 {
    let sub = format!("共 {} 条指令 · 群内发送即可", COMMAND_ROWS.len());
    let mut y = shell_head(c, f, "词意指令", &sub, false);

    for (i, (cmd, extra, desc)) in COMMAND_ROWS.iter().enumerate() {
        let has_u = !extra.is_empty() && !extra.starts_with('[');
        let row_h = if has_u { 74.0 } else { 55.0 };
        let base = y + 15.0 + 19.0;
        let main = format!("{prefix}{cmd}");
        c.text(CX0 + 12.0, base, &main, &f.serif_b, 20.0, INK, 0.06 * 20.0);
        if extra.starts_with('[') {
            let w = c.text_w(&main, &f.serif_b, 20.0, 0.06 * 20.0);
            c.text(CX0 + 12.0 + w + 6.0, base, extra, &f.sans, 13.5, RED, 0.0);
        } else if has_u {
            c.text(
                CX0 + 12.0,
                base + 24.0 + 5.0 + 12.0,
                &format!("亦可 {prefix}{extra}"),
                &f.sans,
                12.5,
                INK3,
                0.0,
            );
        }
        c.text(CX0 + 12.0 + 230.0 + 18.0, base, desc, &f.sans, 15.5, INK2, 0.0);
        y += row_h;
        if i + 1 < COMMAND_ROWS.len() {
            dotted_sep(c, y);
        }
    }

    shell_foot(c, f, y, &format!("发送「{prefix}词意玩法」查看规则"))
}

/// 规则卡：把玩法讲清楚，示例直接用真实的提示行渲染——
/// 说明与实战看到的是同一套版式，不必二次翻译
pub fn rules_card(c: &mut Canvas, f: &Fonts) -> f32 {
    let mut y = shell_head(c, f, "词意玩法", "猜词 · 看名次 · 顺着邻词收网", true);

    // 一 · 目标
    y += 20.0;
    y = draw_section_head(c, f, y, "一", "目标");
    y = draw_sb(c, f, y, "猜出系统每天选定的那个两字词语。全群共用一个词，谁先猜中算谁的。");
    y += 18.0;
    dotted_sep(c, y);

    // 二 · 反馈
    y += 20.0;
    y = draw_section_head(c, f, y, "二", "反馈");
    y = draw_sb(c, f, y, "每猜一个词，若它落在该词的语义排名里，就会得到一个名次与它的左右邻词。名次越小，离答案越近。");
    let sample = [
        HintRow { rank: 14, prev: "器".into(), word: "镯子".into(), next: "玉".into(), fresh: false },
        HintRow { rank: 15, prev: "子".into(), word: "玉佩".into(), next: "东".into(), fresh: false },
        HintRow { rank: 16, prev: "佩".into(), word: "东西".into(), next: "冥".into(), fresh: true },
    ];
    for (i, row) in sample.iter().enumerate() {
        y = draw_hint_row(c, f, y, row, 3000, i + 1 == sample.len());
    }
    y = draw_key(
        c,
        f,
        y,
        &[
            ("？佩", "名次更靠前（更近）的那个词，末字是「佩」", false),
            ("冥？", "名次更靠后（更远）的那个词，首字是「冥」", false),
            ("#15", "语义排名，数字越小离答案越近", false),
            ("新", "本次刚猜出来的那一条，会单独标出", true),
        ],
    );
    y += 18.0;
    dotted_sep(c, y);

    // 三 · 远近
    y += 20.0;
    y = draw_section_head(c, f, y, "三", "远近");
    y = draw_sb(c, f, y, "名次按五档标注，色条越长离答案越近（对数刻度，前排之间的差距也看得出来）。");
    y += 16.0;
    y = draw_tiers(
        c,
        f,
        y,
        &[
            ("咫尺", "#B0342A", "#1 — #10"),
            ("相邻", "#C0662A", "#11 — #50"),
            ("相近", "#A08420", "#51 — #200"),
            ("相关", "#4B7A62", "#201 — #1000"),
            ("天涯", "#5D6C85", "#1000 之后"),
        ],
    );
    y += 18.0;
    dotted_sep(c, y);

    // 四 · 周期
    y += 20.0;
    y = draw_section_head(c, f, y, "四", "周期");
    y = draw_sb(c, f, y, "每日一词，猜中后次日零点换新；系统记录每个人的猜中次数，可用「词意榜」「词意全榜」查看。");

    shell_foot(c, f, y, "发送「词意猜测 词语」即可开局")
}

/// 分区标题：编号印 + 标题
fn draw_section_head(c: &mut Canvas, f: &Fonts, y: f32, num: &str, title: &str) -> f32 {
    c.rrect_fill(CX0 + 2.0, y, 24.0, 24.0, 4.0, RED);
    c.text_center(CX0 + 14.0, y + 12.0, num, &f.sans_b, 13.0, CREAM, 0.0);
    c.text(CX0 + 2.0 + 24.0 + 11.0, mid_baseline(y + 12.0, 21.0), title, &f.serif_b, 21.0, INK, 0.1 * 21.0);
    y + 25.0
}

/// 分区正文：自动折行，行高 1.85
fn draw_sb(c: &mut Canvas, f: &Fonts, y: f32, text: &str) -> f32 {
    let lh = 16.0 * 1.85;
    let lines = c.wrap(text, &f.sans, 16.0, 0.0, CW - 4.0, 6);
    let mut by = y + 12.0 + 14.0;
    for line in &lines {
        c.text(CX0 + 2.0, by, line, &f.sans, 16.0, INK2, 0.0);
        by += lh;
    }
    y + 12.0 + lines.len() as f32 * lh
}

/// 图例键：`（词, 说明, 是否实心印）`
fn draw_key(c: &mut Canvas, f: &Fonts, y: f32, rows: &[(&str, &str, bool)]) -> f32 {
    let mut ky = y + 16.0;
    const BH: f32 = 35.0;
    for (term, desc, solid) in rows {
        if *solid {
            c.rrect_fill(CX0 + 2.0, ky, 96.0, BH, 6.0, RED);
            c.text_center(CX0 + 50.0, ky + BH / 2.0, term, &f.serif_b, 19.0, CREAM, 0.08 * 19.0);
        } else {
            c.rrect_fill(CX0 + 2.0, ky, 96.0, BH, 6.0, RED.with_a(0.07));
            dashed_rect(c, CX0 + 2.0, ky, 96.0, BH, RED.with_a(0.26));
            c.text_center(CX0 + 50.0, ky + BH / 2.0, term, &f.serif_b, 19.0, RED, 0.08 * 19.0);
        }
        c.text(CX0 + 2.0 + 96.0 + 14.0, mid_baseline(ky + BH / 2.0, 15.0), desc, &f.sans, 15.0, INK2, 0.0);
        ky += BH + 10.0;
    }
    ky - 10.0
}

/// 五档距离图例
fn draw_tiers(c: &mut Canvas, f: &Fonts, y: f32, tiers: &[(&str, &str, &str)]) -> f32 {
    let gap = 9.0;
    let tw = (CW - gap * 4.0) / 5.0;
    let h = 60.0;
    for (i, (label, color, range)) in tiers.iter().enumerate() {
        let x = CX0 + (tw + gap) * i as f32;
        let ink = Ink::hex(color);
        c.rrect_fill(x, y, tw, h, 8.0, Ink::rgb(255, 255, 255).with_a(0.5));
        c.rrect_stroke(x, y, tw, h, 8.0, 1.0, SEPIA.with_a(0.20));
        c.rect(x + 1.0, y, tw - 2.0, 2.0, ink);
        c.text_center(x + tw / 2.0, y + 19.0, label, &f.serif_b, 16.0, ink, 0.1 * 16.0);
        c.text_center(x + tw / 2.0, y + 44.0, range, &f.sans, 11.5, INK3, 0.0);
    }
    y + h
}

// ================= 入口 =================

/// 预分配画布高度（逻辑像素）：按内容类型给足上界，绘制完再裁掉空白。
fn alloc_height(reply: &Reply) -> f32 {
    let base = match reply {
        Reply::Board(b) => {
            430.0 + b.rows.len() as f32 * 80.0
                + if b.rows.is_empty() { 160.0 } else { 0.0 }
                + if b.notice.is_some() { 84.0 } else { 0.0 }
        }
        Reply::Win(_) => 780.0,
        Reply::Rank(r) => {
            440.0 + r.items.len() as f32 * 76.0 + if r.items.is_empty() { 160.0 } else { 0.0 }
        }
        Reply::Help => 820.0,
        Reply::Rules => 1560.0,
        Reply::Notice(_) => 0.0,
    };
    (base + 100.0).min(3000.0)
}

/// 按回应类型选卡片并原生绘制成 PNG base64；
/// `Notice` 不出图，返回 None 由调用方走文本；字体不可用同样退回文本。
pub fn render(reply: &Reply, prefix: &str, scale: f64) -> Option<String> {
    let f = Fonts::get()?;
    let s = if scale.is_finite() { scale.clamp(1.0, 4.0) as f32 } else { 3.0 };
    let mut c = Canvas::new(VIEW_W, alloc_height(reply), s);

    let bottom = match reply {
        Reply::Board(board) => board_card(&mut c, f, board),
        Reply::Win(win) => win_card(&mut c, f, win),
        Reply::Rank(rank) => rank_card(&mut c, f, rank),
        Reply::Help => help_card(&mut c, f, prefix),
        Reply::Rules => rules_card(&mut c, f),
        Reply::Notice(_) => return None,
    };

    let img: RgbaImage = c.crop(bottom);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(STANDARD.encode(png))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ciyi::view::RankItem;
    use chrono::{Duration as ChronoDuration, Utc};

    fn sample_board(hidden: usize, notice: Option<&str>) -> Board {
        let words = [
            (7, "环", "手镯", "玉"),
            (14, "器", "镯子", "玉"),
            (15, "子", "玉佩", "东"),
            (137, "佩", "东西", "冥"),
            (864, "西", "物件", "行"),
            (2301, "件", "行头", "衣"),
        ];
        Board {
            notice: notice.map(str::to_string),
            rows: words
                .iter()
                .enumerate()
                .map(|(i, (rank, prev, word, next))| HintRow {
                    rank: *rank,
                    prev: (*prev).into(),
                    word: (*word).into(),
                    next: (*next).into(),
                    fresh: i == 3,
                })
                .collect(),
            hidden,
            guesses: 21,
            hits: 6 + hidden,
            pool: 4831,
        }
    }

    fn sample_win() -> Win {
        Win {
            answer: "东西".into(),
            winner: "夜航船".into(),
            guesses: 23,
            hits: 9,
            started_at: Utc::now() - ChronoDuration::minutes(194),
        }
    }

    fn sample_rank() -> RankBoard {
        RankBoard {
            title: "词意榜".into(),
            subtitle: "本群 · 猜中次数前 6 名".into(),
            items: [("夜航船", 12), ("清风", 9), ("南山", 7), ("拾遗", 4), ("白露", 2), ("无名氏", 1)]
                .iter()
                .map(|(name, score)| RankItem { name: (*name).into(), score: *score })
                .collect(),
        }
    }

    /// 把五种卡片各出一次图，确认都能走完绘制并编码成 PNG：
    ///   CIYI_CARD_DUMP=/tmp/ciyi cargo test ciyi::card -- --ignored
    #[test]
    #[ignore = "需要可用的 CJK 字体，落盘 PNG 供人工核对"]
    fn renders_sample_cards_to_png() {
        let Ok(dir) = std::env::var("CIYI_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let long = "✿ 今天也要元气满满地猜词哦超级无敌长的群昵称 ✿ QwQ";
        let long_win = Win { winner: long.into(), guesses: 7, hits: 3, ..sample_win() };
        let long_rank = RankBoard {
            title: "词意全榜".into(),
            subtitle: "全部群聊 · 猜中次数前 3 名".into(),
            items: vec![
                RankItem { name: long.into(), score: 12 },
                RankItem { name: "无空格的超长英文名".repeat(4), score: 9 },
                RankItem { name: "短名".into(), score: 1 },
            ],
        };

        let cases: Vec<(&str, Reply)> = vec![
            ("board", Reply::Board(sample_board(4, None))),
            ("board_notice", Reply::Board(sample_board(0, Some("「玉佩」已经猜过了")))),
            ("board_empty", Reply::Board(Board {
                notice: None, rows: vec![], hidden: 0, guesses: 3, hits: 0, pool: 4831,
            })),
            ("win", Reply::Win(sample_win())),
            ("win_long_name", Reply::Win(long_win)),
            ("rank", Reply::Rank(sample_rank())),
            ("rank_long_name", Reply::Rank(long_rank)),
            ("help", Reply::Help),
            ("rules", Reply::Rules),
        ];
        for (name, reply) in cases {
            let b64 = render(&reply, "/", 3.0).expect("字体可用时应当出图");
            let bytes = STANDARD.decode(&b64).expect("应是合法 base64");
            assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "{name} 应是 PNG");
            std::fs::write(format!("{dir}/{name}.png"), &bytes).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
    }

    #[test]
    fn notice_never_renders() {
        assert!(render(&Reply::Notice("不在词库中".into()), "/", 3.0).is_none());
    }

    #[test]
    fn heat_falls_off_on_a_log_scale() {
        let (near, mid, far) = (heat(1, 5000), heat(50, 5000), heat(4000, 5000));
        assert!(near > mid && mid > far, "名次越大接近度越低");
        assert!(near > 0.95, "第一名应当接近满格");
        // 线性刻度下 50/5000 会画成 99%，对数刻度要把前排真正拉开
        assert!(mid < 0.6, "名次 50 不该看起来像咬住了答案");
    }

    #[test]
    fn tiers_cover_every_rank() {
        for rank in [1usize, 10, 11, 50, 51, 200, 201, 1000, 1001, 99999] {
            let (label, color) = tier(rank);
            assert!(!label.is_empty() && color.starts_with('#'));
        }
    }


    /// 复现：notice + 10 行 + hidden，最大规模 board 卡是否画得下
    #[test]
    #[ignore]
    fn repro_worst_case_board() {
        let rows: Vec<(usize, &str, &str, &str)> = vec![
            (7, "环", "手镯", "玉"),
            (14, "器", "镯子", "玉"),
            (15, "子", "玉佩", "东"),
            (137, "佩", "东西", "冥"),
            (864, "西", "物件", "行"),
            (2301, "件", "行头", "衣"),
            (4500, "行", "衣物", "穿"),
            (7800, "物", "服装", "面"),
            (12000, "衣", "穿戴", "环"),
            (17000, "穿", "打扮", "天"),
        ];
        let board = Board {
            notice: Some("「衣物」不在词库中。".to_string()),
            rows: rows.iter().enumerate().map(|(i, (rank, prev, word, next))| HintRow {
                rank: *rank, prev: (*prev).into(), word: (*word).into(), next: (*next).into(), fresh: i == 9,
            }).collect(),
            hidden: 1234,
            guesses: 21,
            hits: 10,
            pool: 18054,
        };
        let f = Fonts::get().unwrap();
        let s = 3.0f32;
        let alloc = alloc_height(&Reply::Board(board.clone()));
        let mut c = Canvas::new(VIEW_W, alloc, s);
        let bottom = board_card(&mut c, f, &board);
        eprintln!("WORST alloc={alloc} bottom={bottom}");
        let img = c.crop(bottom);
        eprintln!("WORST final {:?}", img.dimensions());
        image::DynamicImage::ImageRgba8(img)
            .save(std::env::var("CIYI_CARD_DUMP").unwrap() + "/repro_worst.png").unwrap();
        // 关键断言：内容底界不能超过画布，否则会静默截断
        assert!(bottom <= alloc, "内容 {bottom} 超出画布 {alloc}");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(931), "931");
        assert_eq!(group(4831), "4,831");
        assert_eq!(group(1234567), "1,234,567");
    }
}
