//! 把资讯排版成一张卡片图（原生绘制，不走浏览器）。
//!
//! 这张卡以前是 HTML + 无头浏览器截图。资讯推送每天定时跑好几次，每次都要拉起
//! 一个 Chromium 标签页——在一部手机上，那是这个进程最贵的一笔开销，也是唯一
//! 会「因为浏览器起不来」而整条链路失败的一环。而卡片的内容其实是完全结构化的：
//! 序号、标题、几个元信息、一段摘要，外加榜单的分数。**结构化的东西不需要一个
//! 排版引擎**，于是改走框架自己的 [`crate::render::kit`]：纯 CPU、几十毫秒、
//! 没有外部进程，唯一的前提是系统里有一份中日韩字体。
//!
//! 设计取向沿用旧卡：随北京时间自动切换「日读 / 夜读」，也可在配置中固定主题；
//! 四类内容各有一个主色——速递靛蓝、热点橙、日报薄荷绿、模型榜琥珀金，
//! 主色只出现在序号牌、标签、引用左界与分数这几处，版式则与 `/help`、`/ctl`
//! 共用同一套语言。
//!
//! 图片仍旧只承载「读」的部分：链接一概不画进图里。需要查看全文时，用户引用
//! 卡片执行提取指令，再按需取得正文与链接。

use super::api::{DailyBlock, DailyReport, HotTopic, Item, category_label};
use super::leaderboard::{Board, Trend};
use super::render::{RenderOptions, fmt_time, truncate};
use crate::render::Ink;
use crate::render::kit::{self, Block, Bullet, Doc, Entry, Mark, Meta, Score, Theme, Tone};
use chrono::{DateTime, Timelike, Utc};

/// 卡片主色。深浅底各一支：浅底上要压暗才够对比，深底上要提亮才不闷。
#[derive(Clone, Copy)]
pub struct Accent {
    dark: Ink,
    light: Ink,
}

const BRIEF: Accent = Accent {
    dark: Ink::rgb(154, 168, 232),
    light: Ink::rgb(72, 94, 206),
};
const HOT: Accent = Accent {
    dark: Ink::rgb(233, 160, 123),
    light: Ink::rgb(191, 80, 38),
};
const DAILY: Accent = Accent {
    dark: Ink::rgb(114, 201, 174),
    light: Ink::rgb(20, 126, 98),
};
const MODELS: Accent = Accent {
    dark: Ink::rgb(221, 187, 116),
    light: Ink::rgb(158, 94, 0),
};

/// 最终用于出图的主题。`auto` 在 07:00—18:59 使用日读，其余时间使用夜读。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardTheme {
    Light,
    Dark,
}

impl CardTheme {
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "白天",
            Self::Dark => "夜晚",
        }
    }

    fn theme(self, accent: Accent) -> Theme {
        match self {
            Self::Dark => Theme::blueprint().accented(accent.dark),
            Self::Light => Theme::paper().accented(accent.light),
        }
    }
}

pub fn resolve_theme(mode: &str) -> CardTheme {
    resolve_theme_at(mode, Utc::now().with_timezone(&super::render::beijing()))
}

fn resolve_theme_at(mode: &str, now: DateTime<chrono::FixedOffset>) -> CardTheme {
    match mode.trim().to_ascii_lowercase().as_str() {
        "dark" | "night" | "夜晚" | "夜间" => CardTheme::Dark,
        "light" | "day" | "白天" | "日间" => CardTheme::Light,
        "auto" => match (7..19).contains(&now.hour()) {
            true => CardTheme::Light,
            false => CardTheme::Dark,
        },
        _ => CardTheme::Light,
    }
}

/// 清单卡版心。比帮助卡窄一些：资讯是拿来读的，一行太长会丢行。
const WIDTH: f32 = 840.0;
/// 模型榜要多让出一列分数
const BOARD_WIDTH: f32 = 900.0;

/// 一级推送只发图；用户引用图片后按序号或批量提取链接。
const FOOT_HINT: (&str, &str) = ("引用本图提取正文与链接", "/ai提取 全部");
const SOURCE: &str = "AIHOT · aihot.virxact.com";

/// 一张待出图的卡片。
///
/// 出图是纯 CPU 工作，但一张长卡在 3 倍下也要上百毫秒——推送发生在异步任务里，
/// 所以由调用方决定放不放到阻塞线程池上跑，这里只提供同步的入口。
pub struct Card(Doc);

impl Card {
    /// 出图，返回 PNG base64。没有可用的中日韩字体时返回 None，调用方退回纯文本。
    pub fn render(&self, scale: f64) -> Option<String> {
        kit::render(&self.0, scale)
    }
}

/// 一条资讯的元信息：来源 · 分类 · 时间
fn item_meta(item: &Item) -> Vec<Meta> {
    let mut meta = Vec::new();
    if let Some(cat) = item.category.as_deref().filter(|c| !c.trim().is_empty()) {
        meta.push(Meta::Chip(category_label(cat).to_string()));
    }
    if let Some(name) = item
        .source
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta.push(Meta::Plain(name.to_string()));
    }
    // 拿不到原文发布时间时退回收录时间，并如实标注，不冒充发布时间
    let time = item
        .published_at
        .as_deref()
        .and_then(fmt_time)
        .or_else(|| {
            item.discovered_at
                .as_deref()
                .and_then(fmt_time)
                .map(|t| format!("{t} 收录"))
        });
    if let Some(time) = time {
        meta.push(Meta::Plain(time));
    }
    meta
}

fn text_of(value: Option<&String>) -> Option<&str> {
    value.map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn doc(theme: Theme, width: f32, kicker: &str, blocks: Vec<Block>) -> Card {
    Card(Doc {
        theme,
        width,
        kicker: kicker.into(),
        blocks,
        foot: SOURCE.into(),
        hint: (FOOT_HINT.0.into(), FOOT_HINT.1.into()),
    })
}

/// 资讯列表卡片（速递 / 搜索结果共用）
pub fn items_card(
    title: &str,
    subtitle: &str,
    items: &[Item],
    opts: &RenderOptions,
    theme: CardTheme,
) -> Card {
    let entries: Vec<Entry> = items
        .iter()
        .enumerate()
        .map(|(index, item)| Entry {
            rank: format!("{:02}", index + 1),
            top: false,
            title: text_of(item.title.as_ref()).unwrap_or("(无标题)").to_string(),
            mark: None,
            meta: item_meta(item),
            body: text_of(item.summary.as_ref())
                .map(|s| truncate(s, 120))
                .unwrap_or_default(),
            quote: opts
                .show_reason
                .then(|| text_of(item.reason.as_ref()))
                .flatten()
                .map(|reason| ("推荐理由".to_string(), truncate(reason, 90))),
            meter: None,
            score: None,
        })
        .collect();

    let subtitle = match subtitle.is_empty() {
        true => format!("共 {} 条", entries.len()),
        false => format!("{subtitle} · 共 {} 条", entries.len()),
    };
    doc(
        theme.theme(BRIEF),
        WIDTH,
        "AI NEWS",
        vec![
            Block::Title {
                title: title.into(),
                pill: None,
                sub: subtitle,
            },
            Block::Rule,
            Block::Entries(entries),
        ],
    )
}

/// 热点榜卡片：前三名用实心序号牌，其余描边，一眼看出梯队
pub fn hot_topics_card(topics: &[HotTopic], theme: CardTheme) -> Card {
    let entries: Vec<Entry> = topics
        .iter()
        .enumerate()
        .map(|(index, topic)| {
            let rank = topic.rank.unwrap_or((index + 1) as u32);
            let mut meta = Vec::new();
            if let Some(count) = topic.source_count.filter(|c| *c > 0) {
                meta.push(Meta::Chip(format!("{count} 个信源")));
            }
            meta.extend(
                topic
                    .source_names
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .take(3)
                    .map(|s| Meta::Plain(s.to_string())),
            );
            if let Some(time) = topic.latest_at.as_deref().and_then(fmt_time) {
                meta.push(Meta::Plain(format!("最新 {time}")));
            }
            Entry {
                rank: rank.to_string(),
                top: rank <= 3,
                title: text_of(topic.title.as_ref())
                    .unwrap_or("(无标题)")
                    .to_string(),
                mark: None,
                meta,
                body: text_of(topic.summary.as_ref())
                    .map(|s| truncate(s, 120))
                    .unwrap_or_default(),
                quote: None,
                meter: None,
                score: None,
            }
        })
        .collect();

    doc(
        theme.theme(HOT),
        WIDTH,
        "AI HOTLIST",
        vec![
            Block::Title {
                title: "当前热点榜".into(),
                pill: None,
                sub: format!("跨信源聚合 · TOP {}", entries.len()),
            },
            Block::Rule,
            Block::Entries(entries),
        ],
    )
}

/// 模型榜卡片：左名次、中模型与价格、右共识分。
///
/// 共识分是这张图唯一需要「一眼看到」的数字，所以给到右栏的大字与一条同色进度条；
/// 其余信息（厂商、上线日期、价格）压到小字灰阶里，不与之争。
pub fn models_card(board: &Board, max_items: usize, theme: CardTheme) -> Card {
    let shown = &board.entries[..board.entries.len().min(max_items.max(1))];
    let entries: Vec<Entry> = shown
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let rank = model.rank.unwrap_or((index + 1) as u32);
            let trend = model.trend();
            let mark = match trend {
                Trend::Flat => None,
                Trend::Up(_) => Some((trend.marker(), Mark::Up)),
                Trend::Down(_) => Some((trend.marker(), Mark::Down)),
                Trend::New => Some((trend.marker(), Mark::New)),
            };

            let mut meta = Vec::new();
            if let Some(provider) = model.provider_name() {
                meta.push(Meta::Chip(provider.to_string()));
            }
            if let Some(date) = model.released_date() {
                meta.push(Meta::Plain(format!("上线 {date}")));
            }
            if let Some(context) = model.context_text() {
                meta.push(Meta::Plain(format!("上下文 {context}")));
            }
            if let Some(price) = model.price_text() {
                meta.push(Meta::Plain(price));
            }

            let mut notes = Vec::new();
            if let Some(coverage) = model.coverage_text() {
                notes.push(format!("完整度 {coverage}"));
            }
            if let Some(level) = model.confidence_label() {
                notes.push(format!("可信度 {level}"));
            }

            Entry {
                rank: rank.to_string(),
                top: rank <= 3,
                title: model.display_name().to_string(),
                mark,
                meta,
                body: String::new(),
                quote: None,
                meter: model.score.map(|s| s as f32),
                score: Some(Score {
                    value: model.score_text().unwrap_or_else(|| "—".to_string()),
                    notes,
                }),
            }
        })
        .collect();

    let mut subtitle: Vec<String> = Vec::new();
    if let Some(count) = board.source_count {
        subtitle.push(format!("综合 {count} 家公开榜单"));
    }
    if let Some(updated) = board.updated_at.as_deref().filter(|s| !s.is_empty()) {
        subtitle.push(format!("更新于 {updated}"));
    }
    subtitle.push(format!("TOP {}", entries.len()));

    doc(
        theme.theme(MODELS),
        BOARD_WIDTH,
        "MODEL CONSENSUS",
        vec![
            Block::Title {
                title: "AIHOT 大模型排行榜".into(),
                pill: None,
                sub: subtitle.join(" · "),
            },
            Block::Rule,
            Block::Entries(entries),
            Block::Callout {
                tone: Tone::Note,
                text: "共识分由多家公开评测榜单统一折算，只反映公开评测的汇总结果；价格为厂商官网参考价，人民币／百万 Token。".into(),
            },
        ],
    )
}

/// 日报里的一个条目（含其子条目），扁平成带圆点的列表行
fn daily_items(out: &mut Vec<Bullet>, block: &DailyBlock, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let title = text_of(block.title.as_ref());
    let text = text_of(block.text.as_ref());
    if title.is_some() || text.is_some() {
        out.push(Bullet {
            title: title.unwrap_or_default().to_string(),
            text: text.map(|t| truncate(t, 150)).unwrap_or_default(),
        });
        *budget -= 1;
    }
    for child in &block.children {
        daily_items(out, child, budget);
    }
}

/// AI 日报卡片：保留 lead / sections / flashes 的分栏结构
pub fn daily_card(report: &DailyReport, max_blocks: usize, theme: CardTheme) -> Card {
    let title = text_of(report.title.as_ref())
        .unwrap_or("AI 日报")
        .to_string();
    let mut blocks = vec![
        Block::Title {
            title,
            pill: None,
            sub: report.date.as_deref().unwrap_or_default().to_string(),
        },
        Block::Rule,
    ];

    if let Some(lead) = text_of(report.lead.as_ref()) {
        blocks.push(Block::Callout {
            tone: Tone::Info,
            text: truncate(lead, 260),
        });
    }

    let mut budget = max_blocks.max(1);
    for section in &report.sections {
        if budget == 0 {
            break;
        }
        let mut items = Vec::new();
        for child in &section.children {
            daily_items(&mut items, child, &mut budget);
        }
        // 没有子条目的段落，把自身正文当作内容
        if items.is_empty() {
            daily_items(&mut items, section, &mut budget);
        }
        if items.is_empty() {
            continue;
        }
        if let Some(title) = text_of(section.title.as_ref()) {
            blocks.push(Block::Section {
                title: title.to_string(),
                en: String::new(),
                count: format!("{} 条", items.len()),
            });
        }
        blocks.push(Block::Bullets(items));
    }

    if budget > 0 && !report.flashes.is_empty() {
        let mut items = Vec::new();
        for flash in &report.flashes {
            daily_items(&mut items, flash, &mut budget);
        }
        if !items.is_empty() {
            blocks.push(Block::Section {
                title: "快讯".into(),
                en: "FLASH".into(),
                count: format!("{} 条", items.len()),
            });
            blocks.push(Block::Bullets(items));
        }
    }

    doc(theme.theme(DAILY), WIDTH, "AI DAILY", blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一组覆盖各种边界的样本数据：长标题、缺字段、趋势的四种形态。
    #[allow(clippy::type_complexity)]
    fn samples() -> (Vec<Item>, Vec<HotTopic>, DailyReport, Board) {
        use crate::plugins::ai_news::api::{Links, Source};

        let titles = [
            ("Anthropic 发布 Claude 新一代模型，长上下文推理能力显著提升", "Anthropic 官方博客", "ai-models"),
            ("OpenAI 开放 Realtime API 正式版，延迟降至 300ms 以内", "OpenAI Blog", "ai-products"),
            ("研究显示大模型在多步数学推理中仍依赖模式匹配而非符号演算", "arXiv", "paper"),
            ("英伟达下季度数据中心营收指引超预期，AI 芯片需求未见放缓", "路透社", "industry"),
            ("实践总结：用结构化输出把 LLM 接入既有业务系统的六个要点", "少数派", "tip"),
        ];
        let items: Vec<Item> = titles
            .iter()
            .enumerate()
            .map(|(i, (t, src, cat))| Item {
                id: Some((*t).into()),
                title: Some((*t).into()),
                summary: Some("模型在数学、代码与长文档理解等基准上取得明显提升，官方同时公布了新的定价方案与迁移指南，开发者可即刻通过 API 调用。".into()),
                // 一条没有推荐理由：图上不能因此塌掉一块
                reason: (i != 2).then(|| "发布节奏与竞品形成直接对位，对下游应用的选型有实际影响。".to_string()),
                source: Some(Source { name: Some((*src).into()) }),
                links: Links { aihot: Some("https://aihot.virxact.com/i/1".into()), ..Default::default() },
                published_at: Some("2026-08-21T01:20:00Z".into()),
                discovered_at: None,
                category: Some((*cat).into()),
            })
            .collect();

        let topics: Vec<HotTopic> = titles
            .iter()
            .enumerate()
            .map(|(i, (t, src, _))| HotTopic {
                rank: Some(i as u32 + 1),
                title: Some((*t).into()),
                summary: Some("多家媒体在同一时间窗内跟进报道，讨论集中在能力边界与落地成本两方面。".into()),
                source_count: Some(12 - i as u32),
                source_names: vec![(*src).into(), "机器之心".into(), "量子位".into()],
                latest_at: Some("2026-08-21T03:00:00Z".into()),
                links: Links::default(),
            })
            .collect();

        let leaf = |t: &str, s: &str| DailyBlock {
            title: Some(t.into()),
            text: Some(s.into()),
            url: None,
            children: vec![],
        };
        let report = DailyReport {
            date: Some("2026-08-21".into()),
            title: Some("模型更新密集，推理成本继续下探".into()),
            lead: Some("今日焦点集中在两处：头部厂商的模型迭代节奏进一步压缩，以及推理侧价格在一周内出现第二轮下调。".into()),
            links: Links::default(),
            sections: vec![
                DailyBlock {
                    title: Some("模型与研究".into()),
                    text: None,
                    url: None,
                    children: vec![
                        leaf(titles[0].0, "长上下文与工具调用是本次迭代的重点。"),
                        leaf(titles[2].0, "作者用一组对照实验区分了记忆与推理的贡献。"),
                    ],
                },
                DailyBlock {
                    title: Some("产品与行业".into()),
                    text: None,
                    url: None,
                    children: vec![
                        leaf(titles[1].0, "语音场景的端到端延迟首次进入可用区间。"),
                        leaf(titles[3].0, "指引隐含的产能假设值得关注。"),
                    ],
                },
            ],
            flashes: vec![leaf("多家云厂商同步下调推理单价", "降幅集中在 15%—30% 区间。")],
        };

        let models = [
            ("Claude Fable 5", "Anthropic", 89.4, 0.88, "HIGH", Some(1), 67.206, 336.03),
            ("Claude Opus 5", "Anthropic", 86.2, 0.845, "HIGH", Some(0), 33.603, 168.015),
            ("GPT-5.6 Sol", "OpenAI", 83.3, 0.845, "HIGH", Some(-1), 33.603, 201.62),
            ("Kimi K3", "Moonshot AI", 79.9, 0.845, "MEDIUM", None, 8.0, 32.0),
            ("GLM-5.3", "Z.ai", 76.9, 0.60, "LOW", Some(2), 4.0, 12.0),
        ];
        let board = Board {
            updated_at: Some("8月21日 20:00".into()),
            source_count: Some(7),
            entries: models
                .iter()
                .enumerate()
                .map(|(i, (name, provider, score, coverage, confidence, change, input, output))| {
                    crate::plugins::ai_news::leaderboard::ModelEntry {
                        rank: Some(i as u32 + 1),
                        previous_rank: change.map(|_| i as u32 + 1),
                        rank_change: *change,
                        name: Some((*name).into()),
                        provider: Some((*provider).into()),
                        released_at: Some("2026-06-09T00:00:00.000Z".into()),
                        context_window_tokens: Some(1_000_000),
                        input_price_per_million_cny: Some(*input),
                        output_price_per_million_cny: Some(*output),
                        score: Some(*score),
                        coverage: Some(*coverage),
                        confidence: Some((*confidence).into()),
                        ..Default::default()
                    }
                })
                .collect(),
        };
        (items, topics, report, board)
    }

    fn options() -> RenderOptions {
        RenderOptions {
            summary_max_chars: 90,
            show_reason: true,
            show_original_link: false,
        }
    }

    /// 卡片是数据驱动的，坏也坏在数据上：这里只保证「结构对得上」，
    /// 好不好看靠下面那个落盘测试人工核对。
    #[test]
    fn cards_carry_the_data_and_never_the_links() {
        let (items, topics, report, board) = samples();
        let brief = items_card("AI 资讯速递", "过去 24 小时", &items, &options(), CardTheme::Dark);
        let text = format!("{:?}", DebugDoc(&brief.0));
        assert!(text.contains("Anthropic 发布 Claude"), "{text}");
        assert!(text.contains("08-21 09:20"), "时间应换算为北京时间：{text}");
        assert!(text.contains("推荐理由"));
        // 链接只走文本消息，不画进图里
        assert!(!text.contains("aihot.virxact.com/i/1"), "{text}");

        let hot = hot_topics_card(&topics, CardTheme::Light);
        assert!(format!("{:?}", DebugDoc(&hot.0)).contains("12 个信源"));

        let models = models_card(&board, 12, CardTheme::Dark);
        let text = format!("{:?}", DebugDoc(&models.0));
        assert!(text.contains("89.4") && text.contains("完整度 88%"), "{text}");
        assert!(text.contains("↑1") && text.contains("NEW"), "趋势标记应当出现：{text}");

        let daily = daily_card(&report, 12, CardTheme::Light);
        let text = format!("{:?}", DebugDoc(&daily.0));
        assert!(text.contains("快讯") && text.contains("模型与研究"), "{text}");
    }

    /// 把 Doc 里的文字摊平，供上面的结构断言检查。渲染层本身不需要 Debug。
    struct DebugDoc<'a>(&'a Doc);
    impl std::fmt::Debug for DebugDoc<'_> {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for block in &self.0.blocks {
                match block {
                    Block::Title { title, sub, .. } => write!(out, "{title}|{sub}|")?,
                    Block::Section { title, en, count } => write!(out, "{title}|{en}|{count}|")?,
                    Block::Callout { text, .. } => write!(out, "{text}|")?,
                    Block::Bullets(items) => {
                        for b in items {
                            write!(out, "{}|{}|", b.title, b.text)?;
                        }
                    }
                    Block::Entries(entries) => {
                        for e in entries {
                            write!(out, "{}|{}|", e.rank, e.title)?;
                            if let Some((mark, _)) = &e.mark {
                                write!(out, "{mark}|")?;
                            }
                            for m in &e.meta {
                                match m {
                                    Meta::Chip(t) | Meta::Plain(t) => write!(out, "{t}|")?,
                                }
                            }
                            write!(out, "{}|", e.body)?;
                            if let Some((label, text)) = &e.quote {
                                write!(out, "{label}|{text}|")?;
                            }
                            if let Some(score) = &e.score {
                                write!(out, "{}|{}|", score.value, score.notes.join("|"))?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    }

    /// 缺字段、空列表都不该让出图这一路崩掉。
    #[test]
    fn empty_and_partial_inputs_still_produce_a_card() {
        let bare = Item {
            id: None,
            title: None,
            summary: None,
            reason: None,
            source: None,
            links: Default::default(),
            published_at: None,
            discovered_at: None,
            category: None,
        };
        let card = items_card("空卡", "", &[bare], &options(), CardTheme::Dark);
        assert!(format!("{:?}", DebugDoc(&card.0)).contains("(无标题)"));
        let _ = items_card("空卡", "", &[], &options(), CardTheme::Light);
        let _ = hot_topics_card(&[], CardTheme::Dark);
        let _ = models_card(&Board::default(), 12, CardTheme::Dark);
        let _ = daily_card(&DailyReport::default(), 12, CardTheme::Light);
    }

    #[test]
    fn auto_theme_follows_beijing_reading_hours() {
        use chrono::TimeZone;

        let tz = super::super::render::beijing();
        let morning = tz.with_ymd_and_hms(2026, 9, 5, 7, 0, 0).unwrap();
        let evening = tz.with_ymd_and_hms(2026, 9, 5, 19, 0, 0).unwrap();

        assert_eq!(resolve_theme_at("auto", morning), CardTheme::Light);
        assert_eq!(resolve_theme_at("auto", evening), CardTheme::Dark);
        assert_eq!(resolve_theme_at("light", evening), CardTheme::Light);
        assert_eq!(resolve_theme_at("dark", morning), CardTheme::Dark);
        assert_eq!(resolve_theme_at("unexpected", morning), CardTheme::Light);
    }

    /// 把四张卡各出一次图落盘，供人工核对版式：
    ///   AI_NEWS_CARD_DUMP=/tmp/cards cargo test ai_news::card -- --ignored
    #[test]
    #[ignore = "需要可用的 CJK 字体，落盘 PNG 供人工核对"]
    fn renders_sample_cards_to_png() {
        let Ok(dir) = std::env::var("AI_NEWS_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();
        let (items, topics, report, board) = samples();
        let opts = options();

        let cases: Vec<(&str, Card)> = vec![
            ("brief-light", items_card("AI 资讯速递", "过去 24 小时", &items, &opts, CardTheme::Light)),
            ("brief-dark", items_card("AI 资讯速递", "过去 24 小时", &items, &opts, CardTheme::Dark)),
            ("hot-dark", hot_topics_card(&topics, CardTheme::Dark)),
            ("daily-light", daily_card(&report, 12, CardTheme::Light)),
            ("models-dark", models_card(&board, 12, CardTheme::Dark)),
            ("models-light", models_card(&board, 12, CardTheme::Light)),
        ];
        for (name, card) in cases {
            let b64 = card.render(3.0).expect("字体可用时应当出图");
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)
                .expect("应是合法 base64");
            assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "{name} 应是 PNG");
            std::fs::write(format!("{dir}/{name}.png"), &bytes).unwrap();
            println!("{name} 出图 {} 字节", bytes.len());
        }
    }
}
