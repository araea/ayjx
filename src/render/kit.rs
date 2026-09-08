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
    /// 卡片投影。深底上要重才抬得起来，浅底上同一个值就成了一圈脏灰
    pub shadow: Ink,
    /// 上行 / 好转
    pub good: Ink,
    /// 下行 / 变差
    pub bad: Ink,
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
            ink2: Ink::rgb(183, 194, 211),
            ink3: Ink::rgb(148, 159, 178),
            accent: Ink::rgb(93, 230, 201),
            accent_soft: Ink::rgb(93, 230, 201).with_a(0.12),
            off: Ink::rgb(176, 187, 204),
            shadow: Ink::rgb(0, 0, 0).with_a(0.55),
            good: Ink::rgb(117, 201, 149),
            bad: Ink::rgb(227, 138, 141),
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
            ink2: Ink::rgb(192, 192, 198),
            ink3: Ink::rgb(156, 156, 164),
            accent: Ink::rgb(240, 178, 92),
            accent_soft: Ink::rgb(240, 178, 92).with_a(0.12),
            off: Ink::rgb(180, 180, 188),
            shadow: Ink::rgb(0, 0, 0).with_a(0.55),
            good: Ink::rgb(117, 201, 149),
            bad: Ink::rgb(227, 138, 141),
            text_gamma: 0.82,
        }
    }

    /// 白纸：浅底。深色卡在夜里舒服，白天户外反过来——资讯卡按时段在两者间切换。
    ///
    /// 不是把深色主题的数值取反：浅底上细网格要更淡、文字要更黑、投影要更收，
    /// 否则会显得脏。伽马也调低一档，浅底上的深色字本来就偏重。
    pub const fn paper() -> Self {
        Theme {
            shell: Ink::rgb(233, 237, 243),
            card: Ink::rgb(252, 253, 255),
            grid: Ink::rgb(23, 32, 46).with_a(0.022),
            grid_major: Ink::rgb(23, 32, 46).with_a(0.038),
            border: Ink::rgb(23, 32, 46).with_a(0.115),
            surface: Ink::rgb(23, 32, 46).with_a(0.042),
            ink: Ink::rgb(23, 32, 46),
            ink2: Ink::rgb(61, 72, 88),
            ink3: Ink::rgb(122, 133, 150),
            accent: Ink::rgb(14, 130, 112),
            accent_soft: Ink::rgb(14, 130, 112).with_a(0.11),
            off: Ink::rgb(148, 161, 178),
            shadow: Ink::rgb(21, 32, 48).with_a(0.13),
            good: Ink::rgb(22, 122, 82),
            bad: Ink::rgb(186, 60, 68),
            text_gamma: 0.94,
        }
    }

    /// 换一个主色，其余不动。四类资讯各有自己的色相，版式却共用一套。
    pub const fn accented(mut self, accent: Ink) -> Self {
        self.accent = accent;
        self.accent_soft = accent.with_a(0.12);
        self.grid_major = accent.with_a(0.042);
        self
    }
}

// ================= 版式常量 =================

/// 卡片四周的相纸留白
const SHOT: f32 = 22.0;
/// 卡内左右留白
const PADX: f32 = 34.0;
/// 卡内上留白
const PADT: f32 = 34.0;
/// 卡内下留白
const PADB: f32 = 24.0;
/// 坐标纸细网格步长
const GRID: f32 = 26.0;

// ================= 字号表 =================
//
// 量尺与绘制共用同一组字号：改一处就够，不会再出现「按 14 算高度、按 16 画字」的错位。
//
// 字号按 CSS em 像素解释，正文 19—20px，指令 22px；与卡片宽度一起决定聊天中的
// 实际大小。行高、留白和自动换行同步调整，不能只提高 image_scale。

/// 卡片大标题
const FS_TITLE: f32 = 42.0;
const LH_TITLE: f32 = 50.0;
/// 大标题下的副标题
const FS_SUB: f32 = 20.0;
const LH_SUB: f32 = 28.0;
/// 标题右侧的状态药丸
const FS_PILL: f32 = 15.0;
/// 页眉标签与出图时刻
const FS_KICKER: f32 = 15.0;
const FS_STAMP: f32 = 14.0;
/// 分区标题：中文 / 英文代号 / 计数
const FS_SEC: f32 = 23.0;
const FS_SEC_EN: f32 = 12.5;
const FS_SEC_COUNT: f32 = 13.5;
/// 双栏条目：名称 / 配置键 / 说明
const FS_NAME: f32 = 23.0;
const FS_KEY: f32 = 15.5;
const FS_DESC: f32 = 20.0;
/// 指令 chip 与它的说明、别名
const FS_CMD: f32 = 22.0;
const FS_NOTE: f32 = 20.0;
const LH_NOTE: f32 = 29.0;
const FS_ALIAS: f32 = 16.0;
/// 状态行：主字段 / 补充 / 尾注
const FS_ROW: f32 = 21.0;
const FS_ROW_SUB: f32 = 17.0;
const FS_TAIL: f32 = 15.0;
/// 配置与差异的代码块
const FS_CODE: f32 = 19.0;
/// 引导框正文，以及不进框的脚注
const FS_CALL: f32 = 20.0;
const FS_FOOTNOTE: f32 = 14.0;
const LH_FOOTNOTE: f32 = 21.0;
/// 管理说明完整展开，不省略操作条件。
const CALL_LINES: usize = usize::MAX;
/// 页脚说明与指令提示
const FS_FOOT: f32 = 16.0;
/// 数字格
const FS_TILE: f32 = 30.0;
const FS_TILE_LABEL: f32 = 15.5;
/// 圆点列表
const FS_BULLET: f32 = 17.5;
const FS_BULLET_TEXT: f32 = 16.0;

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
    /// 无框小字：免责声明一类的脚注，读不读都行，别抢视线
    Note,
}

/// 条目元信息里的一小格
pub enum Meta {
    /// 有底色的标签（分类、厂商）
    Chip(String),
    /// 纯文字（来源、时间）
    Plain(String),
}

/// 标题后的小标记
#[derive(Clone, Copy, PartialEq)]
pub enum Mark {
    Up,
    Down,
    New,
}

/// 条目右栏的数字：一个大字加两行小注
pub struct Score {
    pub value: String,
    pub notes: Vec<String>,
}

/// 榜单／清单里的一条。
///
/// 资讯、热点、模型榜三张卡的行结构其实是同一个：左边一个序号牌，右边自上而下是
/// 标题、元信息、正文，外加可选的引用、进度条和右栏数字。与其各写一遍，不如让
/// 用不上的部分留空——省下的不只是代码，还有「三张卡看起来不像一家人」的风险。
pub struct Entry {
    /// 左侧序号，空则不画序号牌
    pub rank: String,
    /// 前三名用实心牌，其余描边
    pub top: bool,
    pub title: String,
    /// 标题后的趋势标记
    pub mark: Option<(String, Mark)>,
    pub meta: Vec<Meta>,
    /// 正文摘要，可为空
    pub body: String,
    /// 主色引用块：（小标签, 正文）
    pub quote: Option<(String, String)>,
    /// 0—100 的进度条
    pub meter: Option<f32>,
    /// 右栏数字
    pub score: Option<Score>,
}

/// 带圆点的一条列表项
pub struct Bullet {
    pub title: String,
    pub text: String,
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
    /// 榜单／清单
    Entries(Vec<Entry>),
    /// 带圆点的列表
    Bullets(Vec<Bullet>),
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
const IT_PADX: f32 = 17.0;
const IT_PADY: f32 = 13.0;
/// 名称行（中文名 + 配置键 chip）的高度
const IT_NAME_H: f32 = 26.0;
/// 名称行与说明之间的间距
const IT_NAME_GAP: f32 = 6.0;
const IT_DESC_LH: f32 = 28.0;
const IT_GAP_X: f32 = 16.0;
const IT_GAP_Y: f32 = 11.0;
/// 插件说明完整展开，字号变大时通过增加卡片高度容纳。
const IT_DESC_LINES: usize = usize::MAX;

/// 条目版式：序号牌、右栏与各段行高
const EN_RANK_W: f32 = 38.0;
const EN_RANK_H: f32 = 27.0;
const EN_GAP_X: f32 = 15.0;
const EN_SCORE_W: f32 = 104.0;
const EN_PADT: f32 = 15.0;
const EN_PADB: f32 = 16.0;
const EN_TITLE: f32 = 22.0;
const EN_TITLE_LH: f32 = 31.0;
const EN_TITLE_LINES: usize = 2;
const EN_MARK: f32 = 14.5;
const EN_META_H: f32 = 25.0;
const EN_META_CHIP: f32 = 13.5;
const EN_META_PLAIN: f32 = 14.0;
const EN_BODY: f32 = 16.5;
const EN_BODY_LH: f32 = 25.0;
const EN_BODY_LINES: usize = 4;
const EN_QUOTE: f32 = 16.0;
const EN_QUOTE_LABEL: f32 = 15.0;
const EN_QUOTE_LH: f32 = 24.0;
const EN_RANK: f32 = 16.5;
const EN_SCORE: f32 = 34.0;
const EN_SCORE_NOTE: f32 = 14.0;
const EN_SCORE_NOTE_LH: f32 = 20.0;
const EN_METER_H: f32 = 7.0;
/// 列表项：圆点列表的行高
const BU_TITLE_LH: f32 = 27.0;
const BU_TEXT_LH: f32 = 25.0;

/// 指令 chip 的高度与左右内边距
const CMD_CHIP_H: f32 = 40.0;
const CHIP_PADX: f32 = 15.0;
/// 别名 chip 的高度
const ALIAS_H: f32 = 25.0;
/// 状态行行高
const ROW_H: f32 = 46.0;
/// 数字格高度
const TILE_H: f32 = 82.0;
/// 圆点列表的正文缩进
const BU_INDENT: f32 = 24.0;
const CODE_LH: f32 = 28.0;
const CALL_LH: f32 = 30.0;

impl Block {
    /// 本段占用的高度（含自身上间距）
    fn measure(&self, c: &Canvas, f: &Fonts, cw: f32) -> f32 {
        match self {
            Block::Title { title, sub, pill } => {
                let lines = c
                    .wrap(
                        title,
                        &f.sans_b,
                        FS_TITLE,
                        0.0,
                        title_width(c, f, cw, pill),
                        usize::MAX,
                    )
                    .len() as f32;
                let mut h = lines * LH_TITLE;
                if !sub.is_empty() {
                    h += 14.0 + c.wrap(sub, &f.sans, FS_SUB, 0.0, cw, 3).len() as f32 * LH_SUB;
                }
                h
            }
            Block::Meter(_) => 20.0 + 8.0,
            Block::Rule => 26.0 + 1.0,
            Block::Section { .. } => 24.0 + 28.0 + 10.0,
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
            Block::Rows(rows) => rows.len() as f32 * ROW_H,
            Block::Code(lines) => {
                let n: usize = lines
                    .iter()
                    .map(|l| {
                        c.wrap(l, &f.sans, FS_CODE, 0.0, cw - 44.0, usize::MAX)
                            .len()
                    })
                    .sum();
                16.0 + 14.0 + n as f32 * CODE_LH + 14.0
            }
            Block::Tiles(_) => 18.0 + TILE_H,
            Block::Entries(entries) => entries
                .iter()
                .enumerate()
                .map(|(i, e)| entry_h(c, f, e, cw) + if i > 0 { 1.0 } else { 0.0 })
                .sum(),
            Block::Bullets(bullets) => {
                14.0 + bullets.iter().map(|b| bullet_h(c, f, b, cw)).sum::<f32>()
            }
            Block::Callout { text, tone } => {
                if *tone == Tone::Note {
                    let n = c.wrap(text, &f.sans, FS_FOOTNOTE, 0.0, cw, 4).len() as f32;
                    return 20.0 + n * LH_FOOTNOTE;
                }
                let n = c
                    .wrap(
                        text,
                        &f.sans,
                        FS_CALL,
                        0.0,
                        call_inner(*tone, cw),
                        CALL_LINES,
                    )
                    .len() as f32;
                22.0 + 18.0 + n * CALL_LH + 18.0
            }
            Block::Gap(h) => *h,
        }
    }
}

/// 引导框里文字可用的宽度。占位框居中排，两边各让出一点，比信息框再窄一档。
fn call_inner(tone: Tone, cw: f32) -> f32 {
    if tone == Tone::Empty {
        cw - 52.0
    } else {
        cw - 48.0
    }
}

fn item_heading(c: &Canvas, f: &Fonts, it: &Item, colw: f32) -> (Vec<String>, Vec<String>) {
    let width = colw - IT_PADX * 2.0;
    (
        c.wrap(&it.name, &f.sans_b, FS_NAME, 0.0, width, usize::MAX),
        c.wrap(&it.key, &f.sans, FS_KEY, 0.0, width, usize::MAX),
    )
}

fn item_h(c: &Canvas, f: &Fonts, it: &Item, colw: f32) -> f32 {
    let (names, keys) = item_heading(c, f, it, colw);
    let lines = c
        .wrap(
            &it.desc,
            &f.sans,
            FS_DESC,
            0.0,
            colw - IT_PADX * 2.0,
            IT_DESC_LINES,
        )
        .len();
    IT_PADY * 2.0
        + names.len() as f32 * 30.0
        + keys.len() as f32 * 22.0
        + IT_NAME_GAP
        + lines as f32 * IT_DESC_LH
}

/// 条目正文可用的宽度：扣掉左边的序号牌与右边的数字栏
fn entry_text_w(entry: &Entry, cw: f32) -> f32 {
    let left = if entry.rank.is_empty() {
        0.0
    } else {
        EN_RANK_W + EN_GAP_X
    };
    let right = if entry.score.is_some() {
        EN_SCORE_W + EN_GAP_X
    } else {
        0.0
    };
    (cw - left - right).max(80.0)
}

fn entry_h(c: &Canvas, f: &Fonts, entry: &Entry, cw: f32) -> f32 {
    let tw = entry_text_w(entry, cw);
    // 趋势标记跟在标题后面，把首行能用的宽度让出去一点
    let mark_w = entry
        .mark
        .as_ref()
        .map(|(text, _)| c.text_w(text, &f.sans_b, EN_MARK, 0.0) + 14.0)
        .unwrap_or(0.0);
    let mut h = EN_PADT
        + c.wrap(
            &entry.title,
            &f.sans_b,
            EN_TITLE,
            0.0,
            tw - mark_w,
            EN_TITLE_LINES,
        )
        .len() as f32
            * EN_TITLE_LH;
    if !entry.meta.is_empty() {
        h += 5.0 + EN_META_H;
    }
    if !entry.body.is_empty() {
        h += 7.0
            + c.wrap(&entry.body, &f.sans, EN_BODY, 0.0, tw, EN_BODY_LINES)
                .len() as f32
                * EN_BODY_LH;
    }
    if let Some((label, text)) = &entry.quote {
        h += 10.0 + 12.0 + quote_lines(c, f, label, text, tw).len() as f32 * EN_QUOTE_LH + 12.0;
    }
    if entry.meter.is_some() {
        h += 11.0 + EN_METER_H;
    }
    h += EN_PADB;

    // 右栏比正文高时以右栏为准，否则大字会顶穿下一条
    if let Some(score) = &entry.score {
        let right = EN_PADT + 38.0 + score.notes.len() as f32 * EN_SCORE_NOTE_LH + EN_PADB;
        return h.max(right);
    }
    h
}

/// 引用块里的行。标签与正文拼在一起排，标签只是首行的一个前缀。
fn quote_lines(c: &Canvas, f: &Fonts, label: &str, text: &str, tw: f32) -> Vec<String> {
    let inner = tw - 26.0;
    let indent = if label.is_empty() {
        0.0
    } else {
        c.text_w(label, &f.sans_b, EN_QUOTE_LABEL, 0.0) + 9.0
    };
    let mut lines = c.wrap(text, &f.sans, EN_QUOTE, 0.0, inner - indent, 1);
    let first = lines.first().cloned().unwrap_or_default();
    let consumed = first.chars().count();
    let rest: String = text.chars().skip(consumed).collect();
    let rest = rest.trim_start().to_string();
    if !rest.is_empty() {
        lines.extend(c.wrap(&rest, &f.sans, EN_QUOTE, 0.0, inner, 2));
    }
    lines
}

fn bullet_h(c: &Canvas, f: &Fonts, bullet: &Bullet, cw: f32) -> f32 {
    let tw = cw - BU_INDENT;
    let mut h = 9.0;
    if !bullet.title.is_empty() {
        h += c
            .wrap(&bullet.title, &f.sans_b, FS_BULLET, 0.0, tw, 2)
            .len() as f32
            * BU_TITLE_LH;
    }
    if !bullet.text.is_empty() {
        h += c
            .wrap(&bullet.text, &f.sans, FS_BULLET_TEXT, 0.0, tw, 4)
            .len() as f32
            * BU_TEXT_LH;
    }
    h
}

/// 指令 chip 里的文字总宽（前缀 + 本体）
fn chip_text_w(c: &Canvas, f: &Fonts, cmd: &Cmd, fs: f32) -> f32 {
    c.text_w(&cmd.prefix, &f.sans_b, fs, 0.0) + c.text_w(&cmd.cmd, &f.sans_b, fs, 0.0)
}

/// 长指令保持字号，按实际宽度换行；不省略参数，不缩字。
fn chip_lines(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> Vec<String> {
    c.wrap(
        &format!("{}{}", cmd.prefix, cmd.cmd),
        &f.sans_b,
        FS_CMD,
        0.0,
        cw - CHIP_PADX * 2.0,
        usize::MAX,
    )
}

fn chip_h(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> f32 {
    CMD_CHIP_H + (chip_lines(c, f, cmd, cw).len() - 1) as f32 * 30.0
}

fn alias_lines(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> Vec<String> {
    if cmd.aliases.is_empty() {
        return vec![];
    }
    c.wrap(
        &format!("别名  {}", cmd.aliases.join("  ·  ")),
        &f.sans,
        FS_ALIAS,
        0.0,
        cw - 8.0,
        usize::MAX,
    )
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
    (chip_text_w(c, f, cmd, FS_CMD) + CHIP_PADX * 2.0).min(cw)
}

/// 说明能否与指令同行。
///
/// 短指令右侧本来就是空的，把说明放过去，一屏能多看好几条；
/// 放不下才换行——这比一律换行省掉近一半高度。
fn cmd_inline(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> bool {
    !cmd.note.is_empty()
        && chip_lines(c, f, cmd, cw).len() == 1
        && chip_w(c, f, cmd, cw) + 16.0 + c.text_w(&cmd.note, &f.sans, FS_NOTE, 0.0) <= cw
}

fn cmd_h(c: &Canvas, f: &Fonts, cmd: &Cmd, cw: f32) -> f32 {
    let mut h = 9.0 + chip_h(c, f, cmd, cw);
    if !cmd.note.is_empty() && !cmd_inline(c, f, cmd, cw) {
        h += 7.0
            + c.wrap(&cmd.note, &f.sans, FS_NOTE, 0.0, cw - 8.0, usize::MAX)
                .len() as f32
                * LH_NOTE;
    }
    if !cmd.aliases.is_empty() {
        h += 8.0 + alias_lines(c, f, cmd, cw).len() as f32 * ALIAS_H;
    }
    h + 9.0
}

/// 渲染整张卡片为 PNG base64。字体不可用或超出位图预算时返回 None，调用方退回纯文本。
pub fn render(doc: &Doc, scale: f64) -> Option<String> {
    if !doc.width.is_finite() || doc.cw() < 200.0 {
        return None;
    }
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
    let head_h = 24.0;
    let foot_h = footer_h(&probe, f, doc, cw);
    let card_h = PADT + head_h + body + foot_h + PADB;
    let total_h = card_h + SHOT * 2.0;
    // 完整展开长配置后，分配位图前检查总像素；超大内容交给已有的纯文本兜底。
    let pixels = (doc.width * s).ceil() * (total_h * s).ceil();
    if !pixels.is_finite() || pixels <= 0.0 || pixels > 64_000_000.0 {
        return None;
    }

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
    c.rrect_shadow(SHOT, SHOT + 3.0, cardw, card_h, 22.0, 12.0, t.shadow);
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
        x + 20.0,
        midline(cy, FS_KICKER),
        &doc.kicker,
        &f.sans_b,
        FS_KICKER,
        t.accent,
        0.22 * FS_KICKER,
    );
    c.text_right(
        w - SHOT - PADX,
        midline(cy, FS_STAMP),
        &stamp(),
        &f.sans,
        FS_STAMP,
        t.ink3,
        0.03 * FS_STAMP,
    );
}

fn footer_lines(c: &Canvas, f: &Fonts, doc: &Doc, cw: f32) -> Vec<String> {
    let mut lines = c.wrap(&doc.foot, &f.sans, FS_FOOT, 0.0, cw, usize::MAX);
    let (label, code) = &doc.hint;
    if !code.is_empty() {
        lines.extend(c.wrap(
            &format!("{label}  {code}"),
            &f.sans,
            FS_FOOT,
            0.0,
            cw,
            usize::MAX,
        ));
    }
    lines
}

fn footer_h(c: &Canvas, f: &Fonts, doc: &Doc, cw: f32) -> f32 {
    32.0 + 16.0 + footer_lines(c, f, doc, cw).len() as f32 * 24.0
}

/// 页脚按行展开，长提示不会覆盖左侧说明。
fn draw_foot(c: &mut Canvas, f: &Fonts, doc: &Doc, x: f32, y: f32, cw: f32) {
    let t = &doc.theme;
    c.hline(x, x + cw, y + 32.0, 1.0, t.border, 0.0, 0.0);
    for (i, line) in footer_lines(c, f, doc, cw).iter().enumerate() {
        c.text(
            x,
            y + 48.0 + FS_FOOT + i as f32 * 24.0,
            line,
            &f.sans,
            FS_FOOT,
            if i == 0 { t.ink3 } else { t.ink2 },
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
            draw_section(c, f, t, x, y + 24.0, cw, title, en, count)
        }
        Block::Items(items) => draw_items(c, f, t, x, y, cw, items),
        Block::Cmds(cmds) => draw_cmds(c, f, t, x, y, cw, cmds),
        Block::Rows(rows) => draw_rows(c, f, t, x, y, cw, rows),
        Block::Code(lines) => draw_code(c, f, t, x, y + 16.0, cw, lines),
        Block::Tiles(tiles) => draw_tiles(c, f, t, x, y + 18.0, cw, tiles),
        Block::Entries(entries) => draw_entries(c, f, t, x, y, cw, entries),
        Block::Bullets(bullets) => draw_bullets(c, f, t, x, y + 14.0, cw, bullets),
        Block::Callout { tone, text } => draw_callout(c, f, t, x, y + 22.0, cw, *tone, text),
        Block::Gap(_) => {}
    }
}

fn title_width(c: &Canvas, f: &Fonts, cw: f32, pill: &Option<(String, bool)>) -> f32 {
    cw - pill
        .as_ref()
        .map(|(text, _)| c.text_w(text, &f.sans_b, FS_PILL, 0.0) + 58.0)
        .unwrap_or(0.0)
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
    let lines = c.wrap(
        title,
        &f.sans_b,
        FS_TITLE,
        0.0,
        title_width(c, f, cw, pill),
        usize::MAX,
    );
    let mut by = y + FS_TITLE * 0.95;
    for line in &lines {
        c.text(x, by, line, &f.sans_b, FS_TITLE, t.ink, 0.0);
        by += LH_TITLE;
    }
    if let Some((text, on)) = pill {
        // 药丸挂在标题首行右侧：状态是标题的一部分，不该滚到副标题里
        let (fg, bg, bd) = if *on {
            (t.accent, t.accent_soft, t.accent.with_a(0.3))
        } else {
            (t.off, t.surface, t.border)
        };
        let tw = c.text_w(text, &f.sans_b, FS_PILL, 0.0);
        let pw = tw + 42.0;
        let px = x + c.text_w(&lines[0], &f.sans_b, FS_TITLE, 0.0) + 16.0;
        let pcy = y + 26.0;
        c.rrect_fill(px, pcy - 16.0, pw, 32.0, 16.0, bg);
        c.rrect_stroke(px, pcy - 16.0, pw, 32.0, 16.0, 1.0, bd);
        c.circle_fill(px + 15.0, pcy, 3.5, fg);
        c.text(
            px + 26.0,
            midline(pcy, FS_PILL),
            text,
            &f.sans_b,
            FS_PILL,
            fg,
            0.0,
        );
    }
    if !sub.is_empty() {
        let mut sy = y + lines.len() as f32 * LH_TITLE + 14.0 + FS_SUB;
        for line in c.wrap(sub, &f.sans, FS_SUB, 0.0, cw, 3) {
            c.text(x, sy, &line, &f.sans, FS_SUB, t.ink2, 0.0);
            sy += LH_SUB;
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
    let cy = y + 14.0;
    let mut tx = x;
    c.rect(tx, cy - 9.5, 3.5, 19.0, t.accent);
    tx += 14.0;
    tx += c.text(
        tx,
        midline(cy, FS_SEC),
        title,
        &f.sans_b,
        FS_SEC,
        t.ink,
        0.01 * FS_SEC,
    ) + 12.0;
    if !en.is_empty() {
        tx += c.text(
            tx,
            midline(cy, FS_SEC_EN),
            en,
            &f.sans_b,
            FS_SEC_EN,
            t.ink3,
            0.2 * FS_SEC_EN,
        ) + 14.0;
    }
    let cnt_w = if count.is_empty() {
        0.0
    } else {
        c.text_w(count, &f.sans_b, FS_SEC_COUNT, 0.0) + 14.0
    };
    if x + cw - cnt_w > tx {
        c.hline(tx, x + cw - cnt_w, cy, 1.0, t.border, 6.0, 7.0);
    }
    if !count.is_empty() {
        c.text_right(
            x + cw,
            midline(cy, FS_SEC_COUNT),
            count,
            &f.sans_b,
            FS_SEC_COUNT,
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
                (t.border.with_a(0.30), t.off, t.ink2)
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

            let (names, keys) = item_heading(c, f, it, colw);
            let mut dy = ry + IT_PADY;
            for line in names {
                c.text(
                    ix + IT_PADX,
                    dy + FS_NAME,
                    &line,
                    &f.sans_b,
                    FS_NAME,
                    name_ink,
                    0.0,
                );
                dy += 30.0;
            }
            for line in keys {
                c.text(
                    ix + IT_PADX,
                    dy + FS_KEY,
                    &line,
                    &f.sans,
                    FS_KEY,
                    if it.on { t.accent } else { t.off },
                    0.0,
                );
                dy += 22.0;
            }
            dy += IT_NAME_GAP + FS_DESC;
            for line in c.wrap(
                &it.desc,
                &f.sans,
                FS_DESC,
                0.0,
                colw - IT_PADX * 2.0,
                IT_DESC_LINES,
            ) {
                c.text(ix + IT_PADX, dy, &line, &f.sans, FS_DESC, desc_ink, 0.0);
                dy += IT_DESC_LH;
            }
        }
        ry += tall + IT_GAP_Y;
    }
}

/// 榜单／清单。
///
/// 一条之内的竖向节奏是固定的：标题 → 元信息 → 正文 → 引用 → 进度条，
/// 缺哪段就跳过哪段的间距，于是「只有标题」的一条不会留下一片空白。
fn draw_entries(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, entries: &[Entry]) {
    let mut ry = y;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            // 条与条之间一道极淡的线：不分栏，只给眼睛一个落点
            c.hline(
                x,
                x + cw,
                ry,
                1.0,
                t.border.with_a(t.border.a * 0.6),
                0.0,
                0.0,
            );
            ry += 1.0;
        }
        let h = entry_h(c, f, entry, cw);
        let tw = entry_text_w(entry, cw);
        let tx = if entry.rank.is_empty() {
            x
        } else {
            x + EN_RANK_W + EN_GAP_X
        };

        // 序号牌：前三名实心，其余描边——不必读数字就知道梯队
        if !entry.rank.is_empty() {
            let by = ry + EN_PADT + 1.0;
            if entry.top {
                c.rrect_fill(x, by, EN_RANK_W, EN_RANK_H, 8.0, t.accent);
                c.text_center(
                    x + EN_RANK_W / 2.0,
                    by + EN_RANK_H / 2.0,
                    &entry.rank,
                    &f.sans_b,
                    EN_RANK,
                    t.card,
                    0.0,
                );
            } else {
                c.rrect_fill(x, by, EN_RANK_W, EN_RANK_H, 8.0, t.surface);
                c.rrect_stroke(x, by, EN_RANK_W, EN_RANK_H, 8.0, 1.0, t.border);
                c.text_center(
                    x + EN_RANK_W / 2.0,
                    by + EN_RANK_H / 2.0,
                    &entry.rank,
                    &f.sans_b,
                    EN_RANK,
                    t.ink3,
                    0.0,
                );
            }
        }

        let mark_w = entry
            .mark
            .as_ref()
            .map(|(text, _)| c.text_w(text, &f.sans_b, EN_MARK, 0.0) + 14.0)
            .unwrap_or(0.0);
        let title_lines = c.wrap(
            &entry.title,
            &f.sans_b,
            EN_TITLE,
            0.0,
            tw - mark_w,
            EN_TITLE_LINES,
        );
        let mut cy = ry + EN_PADT;
        for (i, line) in title_lines.iter().enumerate() {
            let baseline = cy + EN_TITLE * 0.98;
            let advance = c.text(tx, baseline, line, &f.sans_b, EN_TITLE, t.ink, 0.0);
            if i == 0
                && let Some((text, mark)) = &entry.mark
            {
                let ink = match mark {
                    Mark::Up => t.good,
                    Mark::Down => t.bad,
                    Mark::New => t.accent,
                };
                c.text(
                    tx + advance + 9.0,
                    baseline - 2.0,
                    text,
                    &f.sans_b,
                    EN_MARK,
                    ink,
                    0.0,
                );
            }
            cy += EN_TITLE_LH;
        }

        if !entry.meta.is_empty() {
            cy += 5.0;
            draw_meta(c, f, t, tx, cy, tw, &entry.meta);
            cy += EN_META_H;
        }

        if !entry.body.is_empty() {
            cy += 7.0;
            for line in c.wrap(&entry.body, &f.sans, EN_BODY, 0.0, tw, EN_BODY_LINES) {
                c.text(tx, cy + EN_BODY, &line, &f.sans, EN_BODY, t.ink2, 0.0);
                cy += EN_BODY_LH;
            }
        }

        if let Some((label, text)) = &entry.quote {
            cy += 10.0;
            let lines = quote_lines(c, f, label, text, tw);
            let qh = 12.0 + lines.len() as f32 * EN_QUOTE_LH + 12.0;
            c.rrect_fill(tx, cy, tw, qh, 8.0, t.accent_soft);
            c.rect(tx, cy + 6.0, 2.5, qh - 12.0, t.accent);
            let mut qy = cy + 12.0;
            for (i, line) in lines.iter().enumerate() {
                let mut lx = tx + 13.0;
                if i == 0 && !label.is_empty() {
                    lx += c.text(
                        lx,
                        qy + EN_QUOTE,
                        label,
                        &f.sans_b,
                        EN_QUOTE_LABEL,
                        t.accent,
                        0.0,
                    ) + 9.0;
                }
                c.text(lx, qy + EN_QUOTE, line, &f.sans, EN_QUOTE, t.ink2, 0.0);
                qy += EN_QUOTE_LH;
            }
            cy += qh;
        }

        if let Some(value) = entry.meter {
            cy += 11.0;
            // 长度直接等于分数，不做二次拉伸：读者量到的就是那个数
            let ratio = (value / 100.0).clamp(0.0, 1.0);
            c.rrect_fill(tx, cy, tw, EN_METER_H, 3.5, t.surface);
            if ratio > 0.0 {
                c.rrect_fill(tx, cy, tw * ratio, EN_METER_H, 3.5, t.accent);
            }
        }

        if let Some(score) = &entry.score {
            let sx = x + cw;
            let mut sy = ry + EN_PADT;
            c.text_right(
                sx,
                sy + 29.0,
                &score.value,
                &f.sans_b,
                EN_SCORE,
                t.accent,
                0.0,
            );
            sy += 38.0;
            for note in &score.notes {
                c.text_right(sx, sy + 13.0, note, &f.sans, EN_SCORE_NOTE, t.ink3, 0.0);
                sy += EN_SCORE_NOTE_LH;
            }
        }

        ry += h;
    }
}

/// 元信息行：chip 有底色，纯文字之间点一个分隔点；排不下就到此为止
fn draw_meta(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, meta: &[Meta]) {
    let cy = y + EN_META_H / 2.0;
    let mut mx = x;
    let mut previous_plain = false;
    for item in meta {
        match item {
            Meta::Chip(text) => {
                let w = c.text_w(text, &f.sans, EN_META_CHIP, 0.0) + 17.0;
                if mx + w > x + cw {
                    return;
                }
                c.rrect_fill(mx, cy - 10.5, w, 21.0, 5.5, t.accent_soft);
                c.text_center(mx + w / 2.0, cy, text, &f.sans, EN_META_CHIP, t.accent, 0.0);
                mx += w + 7.0;
                previous_plain = false;
            }
            Meta::Plain(text) => {
                let w = c.text_w(text, &f.sans, EN_META_PLAIN, 0.0);
                let dot = if previous_plain { 12.0 } else { 0.0 };
                if mx + dot + w > x + cw {
                    return;
                }
                if previous_plain {
                    c.circle_fill(mx + 3.0, cy, 1.5, t.ink3);
                    mx += dot;
                }
                c.text(
                    mx,
                    midline(cy, EN_META_PLAIN),
                    text,
                    &f.sans,
                    EN_META_PLAIN,
                    t.ink3,
                    0.0,
                );
                mx += w + 7.0;
                previous_plain = true;
            }
        }
    }
}

fn draw_bullets(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, bullets: &[Bullet]) {
    let tw = cw - BU_INDENT;
    let mut ry = y;
    for bullet in bullets {
        let mut cy = ry + 9.0;
        c.circle_fill(x + 4.5, cy + 9.5, 3.5, t.accent.with_a(0.75));
        if !bullet.title.is_empty() {
            for line in c.wrap(&bullet.title, &f.sans_b, FS_BULLET, 0.0, tw, 2) {
                c.text(
                    x + BU_INDENT,
                    cy + FS_BULLET,
                    &line,
                    &f.sans_b,
                    FS_BULLET,
                    t.ink,
                    0.0,
                );
                cy += BU_TITLE_LH;
            }
        }
        if !bullet.text.is_empty() {
            for line in c.wrap(&bullet.text, &f.sans, FS_BULLET_TEXT, 0.0, tw, 4) {
                c.text(
                    x + BU_INDENT,
                    cy + FS_BULLET_TEXT,
                    &line,
                    &f.sans,
                    FS_BULLET_TEXT,
                    t.ink2,
                    0.0,
                );
                cy += BU_TEXT_LH;
            }
        }
        ry += bullet_h(c, f, bullet, cw);
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
        let height = chip_h(c, f, cmd, cw);
        c.rrect_fill(x, cy, cw_chip, height, 10.0, t.accent_soft);
        c.rrect_stroke(x, cy, cw_chip, height, 10.0, 1.0, t.accent.with_a(0.18));
        let colors: Vec<bool> = cmd
            .prefix
            .chars()
            .map(|_| true)
            .chain(
                placeholder_runs(&cmd.cmd)
                    .into_iter()
                    .flat_map(|(s, ph)| s.chars().map(move |_| ph)),
            )
            .collect();
        let mut offset = 0;
        for (i, line) in chip_lines(c, f, cmd, cw).iter().enumerate() {
            let base = midline(cy + CMD_CHIP_H / 2.0 + i as f32 * 30.0, FS_CMD);
            let mut px = x + CHIP_PADX;
            for ch in line.chars() {
                let ink = if colors[offset] { t.accent } else { t.ink };
                px += c.text(px, base, &ch.to_string(), &f.sans_b, FS_CMD, ink, 0.0);
                offset += 1;
            }
        }
        let inline = cmd_inline(c, f, cmd, cw);
        if inline {
            c.text(
                x + cw_chip + 16.0,
                midline(cy + CMD_CHIP_H / 2.0, FS_NOTE),
                &cmd.note,
                &f.sans,
                FS_NOTE,
                t.ink2,
                0.0,
            );
        }
        cy += height;

        if !cmd.note.is_empty() && !inline {
            cy += 7.0;
            for line in c.wrap(&cmd.note, &f.sans, FS_NOTE, 0.0, cw - 8.0, usize::MAX) {
                c.text(x + 4.0, cy + FS_NOTE, &line, &f.sans, FS_NOTE, t.ink2, 0.0);
                cy += LH_NOTE;
            }
        }
        if !cmd.aliases.is_empty() {
            cy += 8.0;
            for line in alias_lines(c, f, cmd, cw) {
                c.text(
                    x + 4.0,
                    midline(cy + ALIAS_H / 2.0, FS_ALIAS),
                    &line,
                    &f.sans,
                    FS_ALIAS,
                    t.ink2,
                    0.0,
                );
                cy += ALIAS_H;
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
        let ry = y + i as f32 * ROW_H;
        let cy = ry + ROW_H / 2.0;
        if i % 2 == 1 {
            // 斑马纹：长清单里一行行看下去不串行
            c.rrect_fill(
                x - 8.0,
                ry,
                cw + 16.0,
                ROW_H,
                6.0,
                t.surface.with_a(t.surface.a * 1.1),
            );
        }
        let dot = if row.on {
            t.accent
        } else {
            t.border.with_a(0.45)
        };
        c.rrect_fill(x, cy - 4.5, 9.0, 9.0, 2.5, dot);
        let mut tx = x + 20.0;
        tx += c.text(
            tx,
            midline(cy, FS_ROW),
            &row.main,
            &f.sans_b,
            FS_ROW,
            if row.on { t.ink } else { t.off },
            0.0,
        ) + 9.0;
        if !row.sub.is_empty() {
            c.text(
                tx,
                midline(cy, FS_ROW_SUB),
                &row.sub,
                &f.sans,
                FS_ROW_SUB,
                t.ink3,
                0.0,
            );
        }
        if !row.tail.is_empty() {
            let w = c.text_w(&row.tail, &f.sans_b, FS_TAIL, 0.0) + 18.0;
            c.rrect_fill(x + cw - w, cy - 11.5, w, 23.0, 7.0, t.accent_soft);
            c.text_center(
                x + cw - w / 2.0,
                cy,
                &row.tail,
                &f.sans_b,
                FS_TAIL,
                t.accent,
                0.0,
            );
        }
    }
}

fn draw_code(c: &mut Canvas, f: &Fonts, t: &Theme, x: f32, y: f32, cw: f32, lines: &[String]) {
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|l| c.wrap(l, &f.sans, FS_CODE, 0.0, cw - 44.0, usize::MAX))
        .collect();
    let h = 14.0 + wrapped.len() as f32 * CODE_LH + 14.0;
    c.rrect_fill(x, y, cw, h, 10.0, t.surface.with_a(t.surface.a * 0.8));
    c.rrect_stroke(x, y, cw, h, 10.0, 1.0, t.border);
    c.rrect_fill(x, y + 12.0, 3.0, h - 24.0, 1.5, t.accent.with_a(0.45));
    let mut ly = y + 14.0 + FS_CODE;
    for line in &wrapped {
        draw_code_line(c, f, t, x + 22.0, ly, line);
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
        c.text(x, baseline, line, &f.sans_b, FS_CODE, t.accent, 0.0);
        return;
    }
    // 只认第一个等号：值里再出现等号也不该被当成分隔
    match line.split_once(" = ") {
        Some((key, value)) => {
            let mut px = x;
            px += c.text(px, baseline, key, &f.sans, FS_CODE, t.ink, 0.0);
            px += c.text(px, baseline, " = ", &f.sans, FS_CODE, t.ink3, 0.0);
            c.text(px, baseline, value, &f.sans, FS_CODE, t.ink2, 0.0);
        }
        None => {
            c.text(x, baseline, line, &f.sans, FS_CODE, t.ink2, 0.0);
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
        c.rrect_fill(tx, y, w, TILE_H, 12.0, t.surface);
        c.rrect_stroke(tx, y, w, TILE_H, 12.0, 1.0, t.border);
        c.text_center(
            tx + w / 2.0,
            y + 32.0,
            &tile.value,
            &f.sans_b,
            FS_TILE,
            t.ink,
            0.0,
        );
        c.text_center(
            tx + w / 2.0,
            y + 61.0,
            &tile.label,
            &f.sans,
            FS_TILE_LABEL,
            t.ink3,
            0.14 * FS_TILE_LABEL,
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
    // 脚注不进框：它是「读不读都行」的一段，画上框就成了要读的一段
    if tone == Tone::Note {
        let mut ly = y - 2.0;
        for line in c.wrap(text, &f.sans, FS_FOOTNOTE, 0.0, cw, 4) {
            c.text(
                x,
                ly + FS_FOOTNOTE,
                &line,
                &f.sans,
                FS_FOOTNOTE,
                t.ink3,
                0.0,
            );
            ly += LH_FOOTNOTE;
        }
        return;
    }
    let lines = c.wrap(
        text,
        &f.sans,
        FS_CALL,
        0.0,
        call_inner(tone, cw),
        CALL_LINES,
    );
    let h = 18.0 + lines.len() as f32 * CALL_LH + 18.0;
    match tone {
        Tone::Info => {
            c.rrect_fill(x, y, cw, h, 12.0, t.surface.with_a(t.surface.a * 0.9));
            c.rrect_stroke(x, y, cw, h, 12.0, 1.0, t.border);
            c.rrect_fill(x, y + 12.0, 3.0, h - 24.0, 1.5, t.accent);
            let mut ly = y + 18.0 + FS_CALL;
            for line in &lines {
                c.text(x + 24.0, ly, line, &f.sans, FS_CALL, t.ink2, 0.0);
                ly += CALL_LH;
            }
        }
        Tone::Empty => {
            c.dashed_rect(x, y, cw, h, 1.0, t.border, 6.0, 5.0);
            let mut ly = y + 18.0 + FS_CALL;
            for line in &lines {
                c.text_center(x + cw / 2.0, ly - 5.0, line, &f.sans, FS_CALL, t.ink3, 0.0);
                ly += CALL_LH;
            }
        }
        Tone::Note => unreachable!("脚注在上面已经画完并返回"),
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn long_commands_and_aliases_keep_every_parameter_at_readable_size() {
        let Some(f) = Fonts::get() else {
            return;
        };
        let cmd = Cmd {
            prefix: "!!".into(),
            cmd: "ctl set 插件名称 <很长的配置路径> [多个可选参数] ".repeat(5),
            note: "说明内容".repeat(80),
            aliases: vec!["/这是一个很长的别名".repeat(12), "/第二个别名".into()],
        };
        for scale in [1.0, 3.0] {
            let c = Canvas::new(1.0, 1.0, scale);
            let cw = 568.0;
            let lines = chip_lines(&c, f, &cmd, cw);
            assert_eq!(lines.concat(), format!("{}{}", cmd.prefix, cmd.cmd));
            assert!(lines.len() > 2);
            for line in lines {
                assert!(c.text_w(&line, &f.sans_b, FS_CMD, 0.0) <= cw - CHIP_PADX * 2.0 + 0.1);
            }
            assert_eq!(
                alias_lines(&c, f, &cmd, cw).concat(),
                format!("别名  {}", cmd.aliases.join("  ·  "))
            );
            let note_h = c
                .wrap(&cmd.note, &f.sans, FS_NOTE, 0.0, cw - 8.0, usize::MAX)
                .len() as f32
                * LH_NOTE;
            assert!(cmd_h(&c, f, &cmd, cw) > chip_h(&c, f, &cmd, cw) + note_h);
        }
    }

    #[test]
    fn oversized_cards_fall_back_before_allocating_a_bitmap() {
        let doc = Doc {
            theme: Theme::graphite(),
            width: 680.0,
            kicker: String::new(),
            blocks: vec![Block::Gap(1_000_000.0)],
            foot: String::new(),
            hint: (String::new(), String::new()),
        };
        assert!(render(&doc, 4.0).is_none());
    }

    /// 出图不该在边上留下没画过的地方。
    ///
    /// 画布尺寸与裁剪高度只要对不上一点，边缘就会留一条透明或半透明的带子——
    /// 深色底上肉眼未必立刻看出来，发到群里被别人的背景一衬就很明显。
    /// 右上角有一团有意为之的主色柔光，会溢出到相纸上，所以那一角不参与比色。
    #[test]
    fn the_render_leaves_no_unpainted_edge() {
        if Fonts::get().is_none() {
            return; // 没有字体时 render 本来就返回 None
        }
        let doc = Doc {
            theme: Theme::blueprint(),
            width: 600.0,
            kicker: "TEST".into(),
            blocks: vec![
                Block::Title {
                    title: "边界".into(),
                    pill: None,
                    sub: "检查四边".into(),
                },
                Block::Entries(vec![Entry {
                    rank: "01".into(),
                    top: true,
                    title: "一条足够长的标题，用来把版心撑满并触发折行处理".into(),
                    mark: Some(("↑2".into(), Mark::Up)),
                    meta: vec![Meta::Chip("分类".into()), Meta::Plain("来源".into())],
                    body: "一段正文。".into(),
                    quote: Some(("理由".into(), "一句说明。".into())),
                    meter: Some(72.0),
                    score: Some(Score {
                        value: "89.4".into(),
                        notes: vec!["完整度 88%".into()],
                    }),
                }]),
                Block::Bullets(vec![Bullet {
                    title: "小标题".into(),
                    text: "一行说明。".into(),
                }]),
                Block::Callout {
                    tone: Tone::Note,
                    text: "脚注一行。".into(),
                },
            ],
            foot: "foot".into(),
            hint: ("提示".into(), "/cmd".into()),
        };
        let b64 = render(&doc, 1.0).expect("字体可用时应当出图");
        use base64::{Engine, engine::general_purpose::STANDARD};
        let image = image::load_from_memory(&STANDARD.decode(&b64).unwrap())
            .unwrap()
            .to_rgba8();
        let (w, h) = image.dimensions();
        assert!(w >= 590 && h > 200, "尺寸不对：{w}x{h}");
        assert!(
            image.pixels().all(|p| p.0[3] == 255),
            "出图里有半透明像素，说明有区域没被画到"
        );
        let shell = doc.theme.shell;
        // 柔光在右上角，取左上、左下、右下与底边比色
        for (x, y) in [(0, 0), (0, h - 1), (w - 1, h - 1), (w / 2, h - 1)] {
            let p = image.get_pixel(x, y).0;
            assert_eq!(
                (p[0], p[1], p[2]),
                (shell.r, shell.g, shell.b),
                "({x},{y}) 不是相纸底色"
            );
        }
    }
}
