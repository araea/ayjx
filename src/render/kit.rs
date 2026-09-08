//! 系统类卡片的成品部件。
//!
//! 帮助、插件控制这类「说明书」卡片，版式需求高度重合：一个页眉、若干分区、
//! 分区里排条目或指令、末尾一段引导和页脚。把这套语言固化成 [`Block`]，
//! 各插件只负责把数据列出来，不再各写一遍圆角、间距和折行。
//!
//! 版式按「先量后画」跑：先用一张 1×1 的量尺画布把每个 Block 的高度算准，
//! 累加得到卡片真实高度，再开一张刚好这么高的画布绘制。这样既不会像
//! 「先给足高度再裁」那样在超长内容上静默截断，也不必猜上界。
//!
//! 配色集中在 [`Theme`]：换一张皮不用碰版式代码。

// 绘制函数按 (canvas, fonts, theme, x, y, w, 内容…) 的固定顺序传参，
// 这个形状本身就是可读的；打包成结构体只会在每个调用点多一层噪声
#![allow(clippy::too_many_arguments)]

use super::canvas::{Canvas, Ink};
use super::font::Fonts;
use chrono::{FixedOffset, Utc};

// ================= 主题 =================

/// 一套卡片配色。深浅底都靠这一组值描述，版式代码不关心明暗。
#[derive(Clone, Copy)]
pub struct Theme {
    /// 画布最外层底色（卡片四周的「相纸」）
    pub shell: Ink,
    /// 卡面
    pub card: Ink,
    /// 卡面细网格
    pub grid: Ink,
    /// 每 5 格加重一次的模数格
    pub grid_major: Ink,
    /// 卡面描边、分隔线
    pub border: Ink,
    /// 条目底色
    pub surface: Ink,
    /// 主文字
    pub ink: Ink,
    /// 次要文字（说明）
    pub ink2: Ink,
    /// 弱文字（标签、时间戳）
    pub ink3: Ink,
    /// 强调色
    pub accent: Ink,
    /// 强调色浅底
    pub accent_soft: Ink,
    /// 「停用」灰
    pub off: Ink,
    /// 文字覆盖度伽马，见 [`Canvas::set_text_gamma`]
    pub text_gamma: f32,
}

impl Theme {
    /// 蓝图：深底 + 青绿主色。系统说明书用它——深色在群聊里不刺眼，
    /// 坐标纸底纹与规线把「这是一张图纸」的意思直接说出来。
    pub const fn blueprint() -> Self {
        Theme {
            shell: Ink::rgb(4, 6, 10),
            card: Ink::rgb(8, 11, 17),
            grid: Ink::rgb(255, 255, 255).with_a(0.020),
            grid_major: Ink::rgb(93, 230, 201).with_a(0.045),
            border: Ink::rgb(255, 255, 255).with_a(0.075),
            surface: Ink::rgb(255, 255, 255).with_a(0.030),
            ink: Ink::rgb(255, 255, 255),
            ink2: Ink::rgb(167, 179, 197),
            ink3: Ink::rgb(108, 119, 139),
            accent: Ink::rgb(93, 230, 201),
            accent_soft: Ink::rgb(93, 230, 201).with_a(0.12),
            off: Ink::rgb(152, 163, 181),
            text_gamma: 0.82,
        }
    }

    /// 石墨：深底 + 琥珀主色。控制台用它，与帮助的青绿区分开，
    /// 一眼能认出「这张是管理面板，不是说明书」。
    pub const fn graphite() -> Self {
        Theme {
            shell: Ink::rgb(8, 8, 9),
            card: Ink::rgb(15, 15, 17),
            grid: Ink::rgb(255, 255, 255).with_a(0.018),
            grid_major: Ink::rgb(240, 178, 92).with_a(0.040),
            border: Ink::rgb(255, 255, 255).with_a(0.080),
            surface: Ink::rgb(255, 255, 255).with_a(0.032),
            ink: Ink::rgb(250, 250, 250),
            ink2: Ink::rgb(176, 176, 182),
            ink3: Ink::rgb(120, 120, 128),
            accent: Ink::rgb(240, 178, 92),
            accent_soft: Ink::rgb(240, 178, 92).with_a(0.12),
            off: Ink::rgb(150, 150, 158),
            text_gamma: 0.82,
        }
    }
}

// ================= 版式常量 =================

/// 卡片四周的相纸留白
const SHOT: f32 = 26.0;
/// 卡内左右留白
const PADX: f32 = 40.0;
/// 卡内上留白
const PADT: f32 = 38.0;
/// 卡内下留白
const PADB: f32 = 26.0;
/// 坐标纸细网格步长
const GRID: f32 = 26.0;

// ================= 数据件 =================

/// 双栏清单里的一格
pub struct Item {
    /// 中文显示名
    pub name: String,
    /// 配置键（等宽 chip）
    pub key: String,
    /// 一句话说明
    pub desc: String,
    /// 是否启用（决定主色还是灰）
    pub on: bool,
}

/// 一条指令
pub struct Cmd {
    /// 指令前缀（`/`）。单独着色，把「前缀可配置」这件事在图上说清楚；
    /// 符号指令（`/#`、`~名`）自带写法，这里留空。
    pub prefix: String,
    /// 指令本体，可含 `<必填>` / `[可选]` 占位符
    pub cmd: String,
    /// 用途说明，可为空
    pub note: String,
    /// 别名（已含前缀）
    pub aliases: Vec<String>,
}

/// 状态清单里的一行
pub struct Row {
    /// 左侧状态点是否点亮
    pub on: bool,
    /// 主字段
    pub main: String,
    /// 主字段后的浅色补充（配置键、中文名等）
    pub sub: String,
    /// 右侧尾注（「待重启」之类）
    pub tail: String,
}

/// 数字格
pub struct Tile {
    pub value: String,
    pub label: String,
}

/// 引导框的语气
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    /// 主色左条：正常引导
    Info,
    /// 灰色虚线框、居中：「没有内容」的占位
    Empty,
}

/// 卡片正文的一段。每段自带上间距，调用方只管按顺序列出来。
pub enum Block {
    /// 大标题 +（可选）状态药丸 +（可选）副标题
    Title {
        title: String,
        pill: Option<(String, bool)>,
        sub: String,
    },
    /// 一格一项的实心仪表：数格子就能和下面的清单对上
    Meter(Vec<bool>),
    /// 主色起首的分隔线
    Rule,
    /// 分区标题：编号 + 中文 + 英文代号 + 虚线 + 计数
    Section {
        title: String,
        en: String,
        count: String,
    },
    /// 双栏条目清单
    Items(Vec<Item>),
    /// 指令清单
    Cmds(Vec<Cmd>),
    /// 状态行清单
    Rows(Vec<Row>),
    /// 等宽代码块（配置、差异）
    Code(Vec<String>),
    /// 数字格一排
    Tiles(Vec<Tile>),
    /// 引导 / 占位框
    Callout { tone: Tone, text: String },
    /// 纯粹的垂直留白
    Gap(f32),
}

/// 一张待渲染的卡片
pub struct Doc {
    pub theme: Theme,
    /// 版心总宽（含相纸留白）
    pub width: f32,
    /// 页眉左侧的主色标签
    pub kicker: String,
    pub blocks: Vec<Block>,
    /// 页脚左侧说明
    pub foot: String,
    /// 页脚右侧提示：（说明, 指令）
    pub hint: (String, String),
}

impl Doc {
    /// 内容宽度
    fn cw(&self) -> f32 {
        self.width - SHOT * 2.0 - PADX * 2.0
    }
}

/// 出图时刻（北京时间）
fn stamp() -> String {
    let tz = FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移");
    Utc::now()
        .with_timezone(&tz)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// 垂直居中的文字基线：CJK 字面重心略高于几何中心
fn midline(cy: f32, px: f32) -> f32 {
    cy + 0.36 * px
}

// ================= 度量与绘制 =================

/// 双栏条目的固定内边距与行高
const IT_PADX: f32 = 16.0;
const IT_PADY: f32 = 13.0;
const IT_DESC_LH: f32 = 21.0;
const IT_GAP_X: f32 = 16.0;
const IT_GAP_Y: f32 = 10.0;
/// 说明最多三行——现有最长的一条正好三行，句子不会断在半途
const IT_DESC_LINES: usize = 3;

const CMD_CHIP_H: f32 = 34.0;
const CODE_LH: f32 = 21.0;
const CALL_LH: f32 = 26.0;

impl Block {
    /// 本段占用的高度（含自身上间距）
    fn measure(&self, c: &Canvas, f: &Fonts, cw: f32) -> f32 {
        match self {
            Block::Title { title, sub, .. } => {
                let lines = c.wrap(title, &f.sans_b, 40.0, 0.0, cw - 120.0, 2).len() as f32;
                let mut h = lines * 48.0;
                if !sub.is_empty() {
                    h += 12.0 + c.wrap(sub, &f.sans, 16.5, 0.0, cw, 3).len() as f32 * 25.0;
                }
                h
            }
            Block::Meter(_) => 20.0 + 8.0,
            Block::Rule => 26.0 + 1.0,
            Block::Section { .. } => 26.0 + 24.0 + 12.0,
            Block::Items(items) => {
                let colw = (cw - IT_GAP_X) / 2.0;
                let mut h = 0.0;
                for pair in items.chunks(2) {
                    let tall = pair
                        .iter()
                        .map(|i| item_h(c, f, i, colw))
                        .fold(0.0f32, f32::max);
                    h += tall + IT_GAP_Y;
                }
                (h - IT_GAP_Y).max(0.0)
            }
            Block::Cmds(cmds) => cmds.iter().map(|x| cmd_h(c, f, x, cw)).sum(),
            Block::Rows(rows) => rows.len() as f32 * 34.0,
            Block::Code(lines) => {
                let n: usize = lines
                    .iter()
                    .map(|l| c.wrap(l, &f.sans, 14.0, 0.0, cw - 36.0, 4).len())
                    .sum();
                16.0 + 14.0 + n as f32 * CODE_LH + 14.0
            }
            Block::Tiles(_) => 18.0 + 74.0,
            Block::Callout { text, tone } => {
                let inner = if *tone == Tone::Empty {
                    cw - 48.0
                } else {
                    cw - 44.0
                };
                let n = c.wrap(text, &f.sans, 15.5, 0.0, inner, 6).len() as f32;
                22.0 + 18.0 + n * CALL_LH + 18.0
            }
            Block::Gap(h) => *h,
        }
    }
}

fn item_h(c: &Canvas, f: &Fonts, it: &Item, colw: f32) -> f32 {
    let lines = c
        .wrap(
            &it.desc,
            &f.sans,
            14.0,
            0.0,
            colw - IT_PADX * 2.0,
            IT_DESC_LINES,
        )
        .len() as f32;
    IT_PADY + 23.0 + 6.0 + lines * IT_DESC_LH + IT_PADY
}

/// 指令 chip 里的文字总宽（前缀 + 本体）
fn chip_text_w(c: &Canvas, f: &Fonts, cmd: &Cmd) -> f32 {
    c.text_w(&cmd.prefix, &f.sans_b, 18.0, 0.0) + c.text_w(&cmd.cmd, &f.sans_b, 18.0, 0.0)
}

/// 把指令拆成「普通文字 / 占位符」交替的片段。
/// `<必填>` 与 `[可选]` 连同尖括号方括号一起算作占位符；没有闭合符号时
/// 剩下的按普通文字处理，不吞字。
fn placeholder_runs(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(['<', '[']) {
        let close = if rest.as_bytes()[pos] == b'<' {
            '>'
        } else {
            ']'
        };
        let Some(end) = rest[pos..].find(close) else {
            break;
        };
        if pos > 0 {
            out.push((&rest[..pos], false));
        }
        out.push((&rest[pos..pos + end + close.len_utf8()], true));
        rest = &rest[pos + end + close.len_utf8()..];
    }
    if !rest.is_empty() {
        out.push((rest, false));
    }
    out
}

/// chip 宽度（不超过版心）
fn chip_w(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> f32 {
    (chip_text_w(c, f, cmd) + 26.0).min(cw)
}

/// 说明能否与指令同行。
///
/// 短指令右侧本来就是空的，把说明放过去，一屏能多看好几条；
/// 放不下才换行——这比一律换行省掉近一半高度。
fn cmd_inline(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> bool {
    !cmd.note.is_empty()
        && chip_w(c, f, cmd, cw) + 16.0 + c.text_w(&cmd.note, &f.sans, 15.0, 0.0) <= cw
}

fn cmd_h(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> f32 {
    let mut h = 9.0 + CMD_CHIP_H;
    if !cmd.note.is_empty() && !cmd_inline(c, f, cmd, cw) {
        h += 7.0 + c.wrap(&cmd.note, &f.sans, 15.0, 0.0, cw - 44.0, 3).len() as f32 * 22.0;
    }
    if !cmd.aliases.is_empty() {
        h += 8.0 + 22.0;
    }
    h + 9.0
}

/// 渲染整张卡片为 PNG base64。字体不可用时返回 None，调用方退回纯文本。
pub fn render(doc: &Doc, scale: f64) -> Option<String> {
    let f = Fonts::get()?;
    let s = if scale.is_finite() {
        scale.clamp(1.0, 4.0) as f32
    } else {
        3.0
    };

    // 1) 量尺画布：只用来算度量，不落一个像素
    let probe = Canvas::new(1.0, 1.0, s);
    let cw = doc.cw();
    let body: f32 = doc.blocks.iter().map(|b| b.measure(&probe, f, cw)).sum();

    // 页眉（标签行）+ 正文 + 页脚
    let head_h = 22.0;
    let foot_h = 30.0 + 18.0 + 20.0;
    let card_h = PADT + head_h + body + foot_h + PADB;
    let total_h = card_h + SHOT * 2.0;

    let mut c = Canvas::new(doc.width, total_h, s);
    c.set_text_gamma(doc.theme.text_gamma);
    draw_shell(&mut c, f, doc, card_h);

    let mut y = SHOT + PADT + head_h;
    let x = SHOT + PADX;
    for block in &doc.blocks {
        let h = block.measure(&c, f, cw);
        draw_block(&mut c, f, &doc.theme, block, x, y, cw);
        y += h;
    }
    draw_foot(&mut c, f, doc, x, y, cw);

    c.into_png_base64(total_h)
}

/// 相纸、卡面、坐标纸底纹、四角规线、页眉标签与时间戳
fn draw_shell(c: &mut Canvas, f: &Fonts, doc: &Doc, card_h: f32) {
    let t = &doc.theme;
    let w = doc.width;
    let cardw = w - SHOT * 2.0;
    c.fill(t.shell);

    // 卡片投影：深底上很淡，但足以把卡面从相纸上「抬起来」
    c.rrect_shadow(
        SHOT,
        SHOT + 3.0,
        cardw,
        card_h,
        22.0,
        12.0,
        Ink::rgb(0, 0, 0).with_a(0.55),
    );
    c.rrect_fill(SHOT, SHOT, cardw, card_h, 22.0, t.card);

    // 坐标纸：细网格打底，每 5 格一道主色模数线
    c.line_grid(SHOT, SHOT, cardw, card_h, GRID, 1.0, t.grid);
    c.line_grid(SHOT, SHOT, cardw, card_h, GRID * 5.0, 1.0, t.grid_major);
    // 右上角一团主色柔光，给深底一点纵深
    c.glow(
        SHOT + cardw - 60.0,
        SHOT + 30.0,
        300.0,
        t.accent.with_a(0.055),
    );
    c.rrect_stroke(SHOT, SHOT, cardw, card_h, 22.0, 1.0, t.border);

    // 印前规线：四角各一个直角
    let crop = |c: &mut Canvas, x: f32, y: f32, dx: f32, dy: f32| {
        c.rect(x, y, 15.0 * dx, 2.0, t.accent.with_a(0.42));
        c.rect(x, y, 2.0, 15.0 * dy, t.accent.with_a(0.42));
    };
    let (l, r) = (SHOT + 18.0, SHOT + cardw - 20.0);
    let (tp, bt) = (SHOT + 18.0, SHOT + card_h - 20.0);
    crop(c, l, tp, 1.0, 1.0);
    c.rect(r - 13.0, tp, 15.0, 2.0, t.accent.with_a(0.42));
    c.rect(r, tp, 2.0, 15.0, t.accent.with_a(0.42));
    c.rect(l, bt, 15.0, 2.0, t.accent.with_a(0.42));
    c.rect(l, bt - 13.0, 2.0, 15.0, t.accent.with_a(0.42));
    c.rect(r - 13.0, bt, 15.0, 2.0, t.accent.with_a(0.42));
    c.rect(r, bt - 13.0, 2.0, 15.0, t.accent.with_a(0.42));

    // 页眉：主色圆点 + 标签，右侧出图时刻
    let x = SHOT + PADX;
    let cy = SHOT + PADT + 8.0;
    c.circle_fill(x + 4.5, cy, 4.5, t.accent);
    c.circle_stroke(x + 4.5, cy, 7.0, 1.0, t.accent.with_a(0.28));
    c.text(
        x + 19.0,
        midline(cy, 14.0),
        &doc.kicker,
        &f.sans_b,
        14.0,
        t.accent,
        0.22 * 14.0,
    );
    c.text_right(
        w - SHOT - PADX,
        midline(cy, 13.5),
        &stamp(),
        &f.sans,
        13.5,
        t.ink3,
        0.03 * 13.5,
    );
}

/// 页脚：一道细线，左说明右指令提示
fn draw_foot(c: &mut Canvas, f: &Fonts, doc: &Doc, x: f32, y: f32, cw: f32) {
    let t = &doc.theme;
    let ly = y + 30.0;
    c.hline(x, x + cw, ly, 1.0, t.border, 0.0, 0.0);
    let cy = ly + 19.0;
    c.text(
        x,
        midline(cy, 13.0),
        &doc.foot,
        &f.sans,
        13.0,
        t.ink3,
        0.02 * 13.0,
    );

    let (label, code) = &doc.hint;
    if code.is_empty() {
        return;
    }
    let code_w = c.text_w(code, &f.sans_b, 13.0, 0.0) + 18.0;
    let cx1 = x + cw;
    c.rrect_fill(cx1 - code_w, cy - 12.0, code_w, 24.0, 6.0, t.surface);
    c.rrect_stroke(cx1 - code_w, cy - 12.0, code_w, 24.0, 6.0, 1.0, t.border);
    c.text_center(cx1 - code_w / 2.0, cy, code, &f.sans_b, 13.0, t.ink2, 0.0);
    if !label.is_empty() {
        c.text_right(
            cx1 - code_w - 10.0,
            midline(cy, 13.0),
            label,
            &f.sans,
            13.0,
            t.ink3,
            0.0,
        );
    }
}

fn draw_block(c: &mut Canvas, f: &Fonts, t: &Theme, block: &Block, x: f32, y: f32, cw: f32) {
    match block {
        Block::Title { title, pill, sub } => draw_title(c, f, t, x, y, cw, title, pill, sub),
        Block::Meter(cells) => draw_meter(c, t, x, y + 20.0, cw, cells),
        Block::Rule => {
            let y = y + 26.0;
            // 主色起首，向右淡出：一条有方向的线，比等灰的分隔线更像图纸
            c.hgrad(x, y, cw * 0.55, 1.0, t.accent.with_a(0.55), t.border);
            c.rect(
                x + cw * 0.55,
                y,
                cw * 0.45,
                1.0,
                t.border.with_a(t.border.a * 0.5),
            );
        }
        Block::Section { title, en, count } => {
            draw_section(c, f, t, x, y + 26.0, cw, title, en, count)
        }
        Block::Items(items) => draw_items(c, f, t, x, y, cw, items),
        Block::Cmds(cmds) => draw_cmds(c, f, t, x, y, cw, cmds),
        Block::Rows(rows) => draw_rows(c, f, t, x, y, cw, rows),
        Block::Code(lines) => draw_code(c, f, t, x, y + 16.0, cw, lines),
        Block::Tiles(tiles) => draw_tiles(c, f, t, x, y + 18.0, cw, tiles),
        Block::Callout { tone, text } => draw_callout(c, f, t, x, y + 22.0, cw, *tone, text),
        Block::Gap(_) => {}
    }
}

fn draw_title(
    c: &mut Canvas,
    f: &Fonts,
    t: &Theme,
    x: f32,
    y: f32,
    cw: f32,
    title: &str,
    pill: &Option<(String, bool)>,
    sub: &str,
) {
    let lines = c.wrap(title, &f.sans_b, 40.0, 0.0, cw - 120.0, 2);
    let mut by = y + 38.0;
    for line in &lines {
        c.text(x, by, line, &f.sans_b, 40.0, t.ink, 0.0);
        by += 48.0;
    }
    if let Some((text, on)) = pill {
        // 药丸挂在标题首行右侧：状态是标题的一部分，不该滚到副标题里
        let (fg, bg, bd) = if *on {
            (t.accent, t.accent_soft, t.accent.with_a(0.3))
        } else {
            (t.off, t.surface, t.border)
        };
        let tw = c.text_w(text, &f.sans_b, 13.5, 0.0);
        let pw = tw + 38.0;
        let px = x + c.text_w(&lines[0], &f.sans_b, 40.0, 0.0) + 16.0;
        let pcy = y + 25.0;
        c.rrect_fill(px, pcy - 14.0, pw, 28.0, 14.0, bg);
        c.rrect_stroke(px, pcy - 14.0, pw, 28.0, 14.0, 1.0, bd);
        c.circle_fill(px + 14.0, pcy, 3.5, fg);
        c.text(
            px + 24.0,
            midline(pcy, 13.5),
            text,
            &f.sans_b,
            13.5,
            fg,
            0.0,
        );
    }
    if !sub.is_empty() {
        let mut sy = by - 48.0 + 12.0 + 18.0;
        for line in c.wrap(sub, &f.sans, 16.5, 0.0, cw, 3) {
            c.text(x, sy, &line, &f.sans, 16.5, t.ink2, 0.0);
            sy += 25.0;
        }
    }
}

fn draw_meter(c: &mut Canvas, t: &Theme, x: f32, y: f32, cw: f32, cells: &[bool]) {
    if cells.is_empty() {
        return;
    }
    let gap = 4.0;
    let w = (cw - gap * (cells.len() as f32 - 1.0)) / cells.len() as f32;
    for (i, on) in cells.iter().enumerate() {
        let cx = x + (w + gap) * i as f32;
        let ink = if *on { t.accent } else { t.border };
        c.rrect_fill(cx, y, w, 8.0, 2.0, ink);
    }
}

fn draw_section(
    c: &mut Canvas,
    f: &Fonts,
    t: &Theme,
    x: f32,
    y: f32,
    cw: f32,
    title: &str,
    en: &str,
    count: &str,
) {
    let cy = y + 12.0;
    let mut tx = x;
    c.rect(tx, cy - 8.0, 3.0, 16.0, t.accent);
    tx += 13.0;
    tx += c.text(
        tx,
        midline(cy, 20.0),
        title,
        &f.sans_b,
        20.0,
        t.ink,
        0.01 * 20.0,
    ) + 12.0;
    if !en.is_empty() {
        tx += c.text(
            tx,
            midline(cy, 11.5),
            en,
            &f.sans_b,
            11.5,
            t.ink3,
            0.2 * 11.5,
        ) + 14.0;
    }
    let cnt_w = if count.is_empty() {
        0.0
    } else {
        c.text_w(count, &f.sans_b, 12.5, 0.0) + 14.0
    };
    if x + cw - cnt_w > tx {
        c.hline(tx, x + cw - cnt_w, cy, 1.0, t.border, 6.0, 7.0);
    }
    if !count.is_empty() {
        c.text_right(
            x + cw,
            midline(cy, 12.5),
            count,
            &f.sans_b,
            12.5,
            t.ink3,
            0.0,
        );
    }
}

fn draw_items(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, items: &[Item]) {
    let colw = (cw - IT_GAP_X) / 2.0;
    let mut ry = y;
    for pair in items.chunks(2) {
        let tall = pair
            .iter()
            .map(|i| item_h(c, f, i, colw))
            .fold(0.0f32, f32::max);
        for (col, it) in pair.iter().enumerate() {
            let ix = x + (colw + IT_GAP_X) * col as f32;
            let (accent, name_ink, desc_ink) = if it.on {
                (t.accent, t.ink, t.ink2)
            } else {
                (t.border.with_a(0.30), t.off, t.ink3)
            };
            c.rrect_fill(
                ix,
                ry,
                colw,
                tall,
                12.0,
                if it.on {
                    t.surface
                } else {
                    t.surface.with_a(t.surface.a * 0.45)
                },
            );
            if it.on {
                c.rrect_stroke(ix, ry, colw, tall, 12.0, 1.0, t.border);
            } else {
                // 停用格用虚线框：不必读文字，边框形态就已经把状态说清楚
                c.dashed_rect(ix, ry, colw, tall, 1.0, t.border, 5.0, 4.0);
            }
            // 左侧竖条：启用为主色，停用为暗灰
            c.rrect_fill(ix, ry + IT_PADY, 3.0, tall - IT_PADY * 2.0, 1.5, accent);

            let cy = ry + IT_PADY + 11.5;
            let mut tx = ix + IT_PADX;
            tx += c.text(
                tx,
                midline(cy, 18.5),
                &it.name,
                &f.sans_b,
                18.5,
                name_ink,
                0.0,
            ) + 9.0;
            // 配置键 chip：它是要被原样敲进输入框的字符串，与中文名在形态上分开
            let kw = c.text_w(&it.key, &f.sans, 12.5, 0.0) + 14.0;
            if tx + kw <= ix + colw - IT_PADX {
                c.rrect_fill(
                    tx,
                    cy - 10.0,
                    kw,
                    20.0,
                    6.0,
                    if it.on { t.accent_soft } else { t.surface },
                );
                c.text_center(
                    tx + kw / 2.0,
                    cy,
                    &it.key,
                    &f.sans,
                    12.5,
                    if it.on { t.accent } else { t.ink3 },
                    0.0,
                );
            }
            let mut dy = ry + IT_PADY + 23.0 + 6.0 + 15.0;
            for line in c.wrap(
                &it.desc,
                &f.sans,
                14.0,
                0.0,
                colw - IT_PADX * 2.0,
                IT_DESC_LINES,
            ) {
                c.text(ix + IT_PADX, dy, &line, &f.sans, 14.0, desc_ink, 0.0);
                dy += IT_DESC_LH;
            }
        }
        ry += tall + IT_GAP_Y;
    }
}

fn draw_cmds(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, cmds: &[Cmd]) {
    let mut ry = y;
    for (i, cmd) in cmds.iter().enumerate() {
        let h = cmd_h(c, f, cmd, cw);
        let mut cy = ry + 9.0;
        // 指令 chip：主色浅底 + 描边。指令是要被原样敲进输入框的字符串，
        // 所以单独成块；前缀与 <参数> 另着主色，一眼看出哪里可配、哪里得自己填。
        let cw_chip = chip_w(c, f, cmd, cw);
        c.rrect_fill(x, cy, cw_chip, CMD_CHIP_H, 9.0, t.accent_soft);
        c.rrect_stroke(x, cy, cw_chip, CMD_CHIP_H, 9.0, 1.0, t.accent.with_a(0.18));
        let base = midline(cy + CMD_CHIP_H / 2.0, 18.0);
        let mut px = x + 13.0;
        if !cmd.prefix.is_empty() {
            px += c.text(px, base, &cmd.prefix, &f.sans_b, 18.0, t.accent, 0.0);
        }
        for (seg, is_ph) in placeholder_runs(&cmd.cmd) {
            let ink = if is_ph { t.accent } else { t.ink };
            px += c.text(px, base, seg, &f.sans_b, 18.0, ink, 0.0);
        }
        let inline = cmd_inline(c, f, cmd, cw);
        if inline {
            c.text(
                x + cw_chip + 16.0,
                midline(cy + CMD_CHIP_H / 2.0, 15.0),
                &cmd.note,
                &f.sans,
                15.0,
                t.ink2,
                0.0,
            );
        }
        cy += CMD_CHIP_H;

        if !cmd.note.is_empty() && !inline {
            cy += 7.0;
            for line in c.wrap(&cmd.note, &f.sans, 15.0, 0.0, cw - 44.0, 3) {
                c.text(x + 4.0, cy + 15.0, &line, &f.sans, 15.0, t.ink2, 0.0);
                cy += 22.0;
            }
        }
        if !cmd.aliases.is_empty() {
            cy += 8.0;
            let mut ax = x + 4.0;
            ax += c.text(
                ax,
                midline(cy + 11.0, 13.0),
                "别名",
                &f.sans,
                13.0,
                t.ink3,
                0.0,
            ) + 9.0;
            for alias in &cmd.aliases {
                let w = c.text_w(alias, &f.sans, 13.0, 0.0) + 16.0;
                if ax + w > x + cw {
                    break;
                }
                c.rrect_fill(ax, cy, w, 22.0, 6.0, t.surface);
                c.text_center(ax + w / 2.0, cy + 11.0, alias, &f.sans, 13.0, t.ink2, 0.0);
                ax += w + 8.0;
            }
        }
        ry += h;
        if i + 1 < cmds.len() {
            c.hline(
                x,
                x + cw,
                ry - 1.0,
                1.0,
                t.border.with_a(t.border.a * 0.7),
                0.0,
                0.0,
            );
        }
    }
}

fn draw_rows(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, rows: &[Row]) {
    for (i, row) in rows.iter().enumerate() {
        let ry = y + i as f32 * 34.0;
        let cy = ry + 17.0;
        if i % 2 == 1 {
            // 斑马纹：长清单里一行行看下去不串行
            c.rrect_fill(
                x - 8.0,
                ry,
                cw + 16.0,
                34.0,
                6.0,
                t.surface.with_a(t.surface.a * 1.1),
            );
        }
        let dot = if row.on {
            t.accent
        } else {
            t.border.with_a(0.45)
        };
        c.rrect_fill(x, cy - 4.0, 8.0, 8.0, 2.0, dot);
        let mut tx = x + 18.0;
        tx += c.text(
            tx,
            midline(cy, 16.0),
            &row.main,
            &f.sans_b,
            16.0,
            if row.on { t.ink } else { t.off },
            0.0,
        ) + 9.0;
        if !row.sub.is_empty() {
            c.text(tx, midline(cy, 13.5), &row.sub, &f.sans, 13.5, t.ink3, 0.0);
        }
        if !row.tail.is_empty() {
            let w = c.text_w(&row.tail, &f.sans_b, 12.0, 0.0) + 16.0;
            c.rrect_fill(x + cw - w, cy - 10.0, w, 20.0, 6.0, t.accent_soft);
            c.text_center(
                x + cw - w / 2.0,
                cy,
                &row.tail,
                &f.sans_b,
                12.0,
                t.accent,
                0.0,
            );
        }
    }
}

fn draw_code(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, lines: &[String]) {
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|l| c.wrap(l, &f.sans, 14.0, 0.0, cw - 36.0, 4))
        .collect();
    let h = 14.0 + wrapped.len() as f32 * CODE_LH + 14.0;
    c.rrect_fill(x, y, cw, h, 10.0, t.surface.with_a(t.surface.a * 0.8));
    c.rrect_stroke(x, y, cw, h, 10.0, 1.0, t.border);
    c.rrect_fill(x, y + 12.0, 3.0, h - 24.0, 1.5, t.accent.with_a(0.45));
    let mut ly = y + 14.0 + 14.5;
    for line in &wrapped {
        draw_code_line(c, f, t, x + 20.0, ly, line);
        ly += CODE_LH;
    }
}

/// 一行配置的着色。
///
/// TOML 读起来最费劲的是「哪里是段、哪里是键、哪里是值」，所以只分这三档：
/// 段名走主色，键名走主文字色，等号后的值退到次要色。再多的高亮反而更乱。
fn draw_code_line(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, baseline: f32, line: &str) {
    let body = line.trim_start();
    if body.starts_with('[') {
        c.text(x, baseline, line, &f.sans_b, 14.0, t.accent, 0.0);
        return;
    }
    // 只认第一个等号：值里再出现等号也不该被当成分隔
    match line.split_once(" = ") {
        Some((key, value)) => {
            let mut px = x;
            px += c.text(px, baseline, key, &f.sans, 14.0, t.ink, 0.0);
            px += c.text(px, baseline, " = ", &f.sans, 14.0, t.ink3, 0.0);
            c.text(px, baseline, value, &f.sans, 14.0, t.ink2, 0.0);
        }
        None => {
            c.text(x, baseline, line, &f.sans, 14.0, t.ink2, 0.0);
        }
    }
}

fn draw_tiles(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, tiles: &[Tile]) {
    if tiles.is_empty() {
        return;
    }
    let gap = 12.0;
    let n = tiles.len() as f32;
    let w = (cw - gap * (n - 1.0)) / n;
    for (i, tile) in tiles.iter().enumerate() {
        let tx = x + (w + gap) * i as f32;
        c.rrect_fill(tx, y, w, 74.0, 12.0, t.surface);
        c.rrect_stroke(tx, y, w, 74.0, 12.0, 1.0, t.border);
        c.text_center(
            tx + w / 2.0,
            y + 28.0,
            &tile.value,
            &f.sans_b,
            26.0,
            t.ink,
            0.0,
        );
        c.text_center(
            tx + w / 2.0,
            y + 55.0,
            &tile.label,
            &f.sans,
            12.0,
            t.ink3,
            0.14 * 12.0,
        );
    }
}

fn draw_callout(
    c: &mut Canvas,
    f: &Fonts,
    t: &Theme,
    x: f32,
    y: f32,
    cw: f32,
    tone: Tone,
    text: &str,
) {
    let inner = if tone == Tone::Empty {
        cw - 48.0
    } else {
        cw - 44.0
    };
    let lines = c.wrap(text, &f.sans, 15.5, 0.0, inner, 6);
    let h = 18.0 + lines.len() as f32 * CALL_LH + 18.0;
    match tone {
        Tone::Info => {
            c.rrect_fill(x, y, cw, h, 12.0, t.surface.with_a(t.surface.a * 0.9));
            c.rrect_stroke(x, y, cw, h, 12.0, 1.0, t.border);
            c.rrect_fill(x, y + 12.0, 3.0, h - 24.0, 1.5, t.accent);
            let mut ly = y + 18.0 + 16.0;
            for line in &lines {
                c.text(x + 22.0, ly, line, &f.sans, 15.5, t.ink2, 0.0);
                ly += CALL_LH;
            }
        }
        Tone::Empty => {
            c.dashed_rect(x, y, cw, h, 1.0, t.border, 6.0, 5.0);
            let mut ly = y + 18.0 + 16.0;
            for line in &lines {
                c.text_center(x + cw / 2.0, ly - 5.0, line, &f.sans, 15.5, t.ink3, 0.0);
                ly += CALL_LH;
            }
        }
    }
}
