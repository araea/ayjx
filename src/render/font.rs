//! 原生绘制用的字体装载与字形回退。
//!
//! 一次进程内加载四张「字面」：宋体 / 黑体 × 常规 / 加粗。每张字面不是单个
//! 字体文件，而是一条**回退链**——首选字缺字时顺着链往下找，链上都没有才
//! 放弃这个字符。链的存在是为了两类真实内容：
//!   - 群昵称里的 emoji 与生僻字：以前落到 `.notdef`，图上是一串空心豆腐块；
//!   - 拉丁与符号：CJK 字体的西文字形往往偏窄偏怪，先让专门的字体接手。
//!
//! 找不到任何可用字体时 [`Fonts::get`] 返回 `None`，调用方退回纯文本，
//! 功能不受影响。

use ab_glyph::{Font, FontVec, GlyphId};

/// 一张「字面」：首选字体 + 回退链。
///
/// 绘制时逐字符查链，`glyph` 给出第一个真正含有该字形的字体。
pub struct Face {
    /// 至少一个元素；`chain[0]` 是首选字体，度量（行高等）以它为准
    chain: Vec<FontVec>,
}

impl Face {
    fn new(primary: FontVec) -> Self {
        Face {
            chain: vec![primary],
        }
    }

    /// 追加一个回退字体（重复追加同一份数据没有害处，只是多占内存，
    /// 所以调用方按「首选 → 同族异重 → 异族 → 符号/emoji」的顺序只加一次）
    fn push(&mut self, font: FontVec) {
        self.chain.push(font);
    }

    /// 度量用的首选字体
    pub fn primary(&self) -> &FontVec {
        &self.chain[0]
    }

    /// 找出真正含有 `ch` 的字体与字形号。
    ///
    /// `glyph_id` 对缺字返回 `GlyphId(0)`（`.notdef`，多数字体画成空心方框），
    /// 所以这里显式跳过 0 —— 宁可这个字符不画，也不要在图上留一排豆腐。
    pub fn glyph(&self, ch: char) -> Option<(&FontVec, GlyphId)> {
        for font in &self.chain {
            let id = font.glyph_id(ch);
            if id.0 != 0 {
                return Some((font, id));
            }
        }
        None
    }
}

/// 两个字族 × 两档字重。宋体管汉字与标题，黑体管数字与元信息；
/// Bold 用于标题与数字，Regular 用于正文。
pub struct Fonts {
    pub serif_b: Face,
    pub serif: Face,
    pub sans_b: Face,
    pub sans: Face,
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

/// 回退链末端：符号、箭头、几何图形、黑白 emoji。
/// 彩色 emoji（CBDT 位图）没有轮廓，ab_glyph 画不出来，所以优先黑白字体；
/// 一个都没有时那些字符就留空，仍好过豆腐块。
const SYMBOL_FILES: &[&str] = &[
    "/system/fonts/NotoSansSymbols-Regular-Subsetted.ttf",
    "/system/fonts/NotoSansSymbols-Regular-Subsetted2.ttf",
    "/system/fonts/NotoSansSymbols2-Regular.ttf",
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
    /// 进程内加载一次；环境里一个可用字体都没有时返回 None。
    pub fn get() -> Option<&'static Fonts> {
        FONTS.get_or_init(Fonts::load).as_ref()
    }

    fn load() -> Option<Fonts> {
        let db = load_db();
        // 单个字重的首选字体：族名查询 → 文件兜底 → 换字族再来一遍
        let pick =
            |own_fam: &[&str], own_files: &[&str], alt_fam: &[&str], alt_files: &[&str], w| {
                load_family(&db, own_fam, w)
                    .or_else(|| load_files(own_files))
                    .or_else(|| load_family(&db, alt_fam, w))
                    .or_else(|| load_files(alt_files))
            };
        let serif = |w| pick(SERIF_FAMILIES, SERIF_FILES, SANS_FAMILIES, SANS_FILES, w);
        let sans = |w| pick(SANS_FAMILIES, SANS_FILES, SERIF_FAMILIES, SERIF_FILES, w);

        // 回退链公用的两个「广覆盖」字体：黑体常规覆盖面最大，符号字体兜底
        let broad = || {
            load_family(&db, SANS_FAMILIES, fontdb::Weight::NORMAL)
                .or_else(|| load_files(SANS_FILES))
        };
        let symbols = || load_files(SYMBOL_FILES);

        let chain = |primary: FontVec| {
            let mut face = Face::new(primary);
            if let Some(f) = broad() {
                face.push(f);
            }
            if let Some(f) = symbols() {
                face.push(f);
            }
            face
        };

        Some(Fonts {
            serif_b: chain(serif(fontdb::Weight::BOLD)?),
            serif: chain(serif(fontdb::Weight::NORMAL)?),
            sans_b: chain(sans(fontdb::Weight::BOLD)?),
            sans: chain(sans(fontdb::Weight::NORMAL)?),
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

/// 按族名与字重加载字体。
fn load_family(
    db: &fontdb::Database,
    families: &[&str],
    weight: fontdb::Weight,
) -> Option<FontVec> {
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
