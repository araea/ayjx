//! 把 AIHOT 的接口数据渲染成适合群聊阅读的纯文本。
//!
//! 原则：
//!   - 时间统一换算为北京时间（UTC+8）后再展示；
//!   - `publishedAt` 为空时回退 `discoveredAt`，并明确标注为「收录」，不冒充原文发布时间；
//!   - `summary` / `reason` 可能为 null，判空后再展示，绝不编造；
//!   - 主链接用站内页 `links.aihot`，仅在配置开启时附第三方原文；
//!   - 保持服务端返回顺序，不按 `score` 自行重排。
//!
//! 渲染结果统一用 [`Rendered`] 表示，拆成「头 / 逐条 / 尾」三段：
//! 内容短时合成一条纯文本发送；超过阈值时按条目打包成合并转发的节点，
//! 群里只占一个折叠卡片，不会刷屏。

use super::api::{DailyBlock, DailyReport, HotTopic, Item, category_label};
use super::leaderboard::{self, Board};
use chrono::{DateTime, FixedOffset};

const DIVIDER: &str = "———————————————";

/// 一次渲染的产物。分段保存，以便按需要合成纯文本或拆成转发节点。
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    /// 标题行
    pub header: String,
    /// 逐条正文，每段自身不带首尾空行
    pub entries: Vec<String>,
    /// 落款（数据来源等）
    pub footer: String,
}

impl Rendered {
    /// 纯提示文本（错误、空结果等），永远按单条纯文本发送
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            header: text.into(),
            entries: Vec::new(),
            footer: String::new(),
        }
    }

    /// 合成单条纯文本
    pub fn to_text(&self) -> String {
        if self.entries.is_empty() {
            return self.header.clone();
        }
        let mut out = String::with_capacity(self.char_count() * 3);
        out.push_str(&self.header);
        out.push('\n');
        out.push_str(DIVIDER);
        out.push('\n');
        for entry in &self.entries {
            out.push_str(entry);
            out.push_str("\n\n");
        }
        out.push_str(DIVIDER);
        if !self.footer.is_empty() {
            out.push('\n');
            out.push_str(&self.footer);
        }
        out
    }

    /// 按字符数估算整条消息的体量（判断是否该转成合并转发）
    pub fn char_count(&self) -> usize {
        self.header.chars().count()
            + self.footer.chars().count()
            + self
                .entries
                .iter()
                .map(|e| e.chars().count() + 2)
                .sum::<usize>()
    }

    /// 把条目贪心打包成若干节点正文：标题并入首个节点，落款单独成节。
    ///
    /// `node_max_chars` 只是软上限——单条超长的条目不会被切断，
    /// 宁可让某个节点长一点，也不在句子中间断开。
    pub fn nodes(&self, node_max_chars: usize) -> Vec<String> {
        if self.entries.is_empty() {
            return vec![self.to_text()];
        }

        let budget = node_max_chars.max(60);
        let mut nodes: Vec<String> = Vec::new();
        let mut current = String::new();

        if !self.header.is_empty() {
            current.push_str(&self.header);
        }

        for entry in &self.entries {
            let entry = entry.trim_end();
            if entry.is_empty() {
                continue;
            }
            let would_be = current.chars().count() + entry.chars().count();
            if !current.is_empty() && would_be > budget {
                nodes.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(entry);
        }

        if !current.is_empty() {
            nodes.push(current);
        }
        if !self.footer.is_empty() {
            nodes.push(self.footer.clone());
        }
        nodes
    }
}

/// 北京时间（UTC+8）
pub(super) fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移")
}

/// ISO8601 → `MM-DD HH:MM`（北京时间）
pub(super) fn fmt_time(iso: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.with_timezone(&beijing()).format("%m-%d %H:%M").to_string())
}

/// 按字符（而非字节）截断，避免切坏中文
pub(super) fn truncate(text: &str, max_chars: usize) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let head: String = cleaned.chars().take(max_chars).collect();
    format!("{}…", head.trim_end())
}

/// 一条资讯的时间行：来源 · 时间（无法取得原文时间时标注为收录时间）
fn meta_line(item: &Item) -> Option<String> {
    let source = item
        .source
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let time = item
        .published_at
        .as_deref()
        .and_then(fmt_time)
        .map(|t| format!("{} 发布", t))
        .or_else(|| {
            item.discovered_at
                .as_deref()
                .and_then(fmt_time)
                .map(|t| format!("{} 收录", t))
        });

    let category = item.category.as_deref().map(category_label);

    let parts: Vec<String> = [source.map(str::to_string), category.map(str::to_string), time]
        .into_iter()
        .flatten()
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

pub struct RenderOptions {
    pub summary_max_chars: usize,
    pub show_reason: bool,
    pub show_original_link: bool,
}

/// 资讯列表（速递 / 搜索结果共用）
pub fn render_items(header: &str, items: &[Item], opts: &RenderOptions) -> Rendered {
    let mut entries = Vec::with_capacity(items.len());

    for (idx, item) in items.iter().enumerate() {
        let mut out = String::new();
        let title = item.title.as_deref().unwrap_or("(无标题)").trim();
        out.push_str(&format!("{}. {}", idx + 1, title));

        if let Some(meta) = meta_line(item) {
            out.push_str(&format!("\n   {}", meta));
        }
        if let Some(summary) = item.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("\n   {}", truncate(summary, opts.summary_max_chars)));
        }
        if opts.show_reason
            && let Some(reason) = item.reason.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            out.push_str(&format!("\n   💡 {}", truncate(reason, opts.summary_max_chars)));
        }
        if let Some(link) = item.links.aihot.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("\n   🔗 {}", link));
        }
        if opts.show_original_link
            && let Some(orig) = item.links.original.as_deref().filter(|s| !s.is_empty())
        {
            out.push_str(&format!("\n   📄 {}", orig));
        }
        entries.push(out);
    }

    Rendered {
        header: header.to_string(),
        entries,
        footer: format!("{} · 共 {} 条", super::api::ATTRIBUTION, items.len()),
    }
}

/// 热点榜：按 rank 展示「第 N 名」，不展示或推算热度值
pub fn render_hot_topics(topics: &[HotTopic]) -> Rendered {
    let mut entries = Vec::with_capacity(topics.len());

    for (idx, topic) in topics.iter().enumerate() {
        let mut out = String::new();
        let rank = topic.rank.unwrap_or((idx + 1) as u32);
        let title = topic.title.as_deref().unwrap_or("(无标题)").trim();
        out.push_str(&format!("第 {} 名 {}", rank, title));

        let mut meta: Vec<String> = Vec::new();
        if let Some(count) = topic.source_count.filter(|c| *c > 0) {
            meta.push(format!("{} 个信源", count));
        }
        let names: Vec<&str> = topic
            .source_names
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .take(3)
            .collect();
        if !names.is_empty() {
            meta.push(names.join("、"));
        }
        if let Some(t) = topic.latest_at.as_deref().and_then(fmt_time) {
            meta.push(format!("最新 {}", t));
        }
        if !meta.is_empty() {
            out.push_str(&format!("\n   {}", meta.join(" · ")));
        }

        if let Some(summary) = topic.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("\n   {}", truncate(summary, 80)));
        }
        if let Some(link) = topic.links.primary() {
            out.push_str(&format!("\n   🔗 {}", link));
        }
        entries.push(out);
    }

    Rendered {
        header: "🔥 AI 当前热点榜".to_string(),
        entries,
        footer: super::api::ATTRIBUTION.to_string(),
    }
}

/// 模型榜：一条一段，先名次与模型名，再共识分与证据情况，最后上线日期与价格
pub fn render_models(board: &Board, max_items: usize) -> Rendered {
    let shown = &board.entries[..board.entries.len().min(max_items.max(1))];
    let mut entries = Vec::with_capacity(shown.len());

    for (idx, model) in shown.iter().enumerate() {
        let rank = model.rank.unwrap_or((idx + 1) as u32);
        let mut out = String::new();

        let trend = model.trend().marker();
        let head = match model.provider_name() {
            Some(provider) => format!("第 {} 名 {}（{}）", rank, model.display_name(), provider),
            None => format!("第 {} 名 {}", rank, model.display_name()),
        };
        out.push_str(&head);
        if !trend.is_empty() {
            out.push_str(&format!(" {}", trend));
        }

        let mut score_line: Vec<String> = Vec::new();
        if let Some(score) = model.score_text() {
            score_line.push(format!("共识分 {}", score));
        }
        if let Some(coverage) = model.coverage_text() {
            score_line.push(format!("完整度 {}", coverage));
        }
        if let Some(level) = model.confidence_label() {
            score_line.push(format!("可信度{}", level));
        }
        if !score_line.is_empty() {
            out.push_str(&format!("\n   {}", score_line.join(" · ")));
        }

        let mut meta: Vec<String> = Vec::new();
        if let Some(date) = model.released_date() {
            meta.push(format!("上线 {}", date));
        }
        if let Some(ctx_len) = model.context_text() {
            meta.push(format!("上下文 {}", ctx_len));
        }
        if let Some(price) = model.price_text() {
            meta.push(price);
        }
        if !meta.is_empty() {
            out.push_str(&format!("\n   {}", meta.join(" · ")));
        }

        entries.push(out);
    }

    let mut footer = String::from(leaderboard::ATTRIBUTION);
    footer.push_str(&format!("\n🔗 {}", leaderboard::PAGE_URL));
    footer.push_str("\n价格为厂商官网参考价，人民币／百万 Token；共识分只反映公开评测的汇总结果。");

    Rendered {
        header: models_header(board),
        entries,
        footer,
    }
}

/// 模型榜标题行：带上「汇总几家榜单」与站点标注的更新时间
pub(super) fn models_header(board: &Board) -> String {
    let mut header = String::from("🏆 AIHOT 大模型排行榜");
    let mut meta: Vec<String> = Vec::new();
    if let Some(count) = board.source_count {
        meta.push(format!("综合 {} 家公开榜单", count));
    }
    if let Some(updated) = board.updated_at.as_deref().filter(|s| !s.is_empty()) {
        meta.push(format!("更新于 {}", updated));
    }
    if !meta.is_empty() {
        header.push_str(&format!("\n{}", meta.join(" · ")));
    }
    header
}

fn render_block(out: &mut String, block: &DailyBlock, depth: usize, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let indent = "  ".repeat(depth);

    if let Some(title) = block.title.as_deref() {
        let marker = if depth == 0 { "▍" } else { "· " };
        out.push_str(&format!("{}{}{}\n", indent, marker, title));
        *budget -= 1;
    }
    if let Some(text) = block.text.as_deref() {
        out.push_str(&format!("{}  {}\n", indent, truncate(text, 100)));
    }
    if let Some(url) = block.url.as_deref() {
        out.push_str(&format!("{}  🔗 {}\n", indent, url));
    }

    for child in &block.children {
        render_block(out, child, depth + 1, budget);
    }
}

/// AI 日报：保留 lead / sections / flashes 的原有结构，不重排成普通列表
pub fn render_daily(report: &DailyReport, max_blocks: usize) -> Rendered {
    let header = match (report.date.as_deref(), report.title.as_deref()) {
        (Some(date), Some(title)) => format!("📰 AI 日报 · {}\n{}", date, title),
        (Some(date), None) => format!("📰 AI 日报 · {}", date),
        (None, Some(title)) => format!("📰 AI 日报 · {}", title),
        (None, None) => "📰 AI 日报".to_string(),
    };

    let mut entries: Vec<String> = Vec::new();

    if let Some(lead) = report.lead.as_deref() {
        entries.push(truncate(lead, 220));
    }

    let mut budget = max_blocks.max(1);
    for section in &report.sections {
        if budget == 0 {
            break;
        }
        let mut block = String::new();
        render_block(&mut block, section, 0, &mut budget);
        let block = block.trim_end();
        if !block.is_empty() {
            entries.push(block.to_string());
        }
    }

    if budget > 0 && !report.flashes.is_empty() {
        let mut block = String::from("⚡ 快讯\n");
        for flash in &report.flashes {
            if budget == 0 {
                break;
            }
            render_block(&mut block, flash, 1, &mut budget);
        }
        let block = block.trim_end();
        if !block.is_empty() {
            entries.push(block.to_string());
        }
    }

    if let Some(link) = report.links.primary() {
        entries.push(format!("完整日报：{}", link));
    }

    Rendered {
        header,
        entries,
        footer: super::api::ATTRIBUTION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ai_news::api::{Links, Source};

    fn sample_item() -> Item {
        Item {
            id: Some("1".into()),
            title: Some("某模型发布".into()),
            summary: Some("这是一段摘要".into()),
            reason: Some("值得关注的理由".into()),
            source: Some(Source {
                name: Some("官方博客".into()),
            }),
            links: Links {
                aihot: Some("https://aihot.virxact.com/i/1".into()),
                original: Some("https://example.com/post".into()),
                story: None,
            },
            published_at: Some("2026-08-21T01:00:00Z".into()),
            discovered_at: Some("2026-08-21T02:00:00Z".into()),
            category: Some("ai-models".into()),
        }
    }

    #[test]
    fn truncates_by_chars_not_bytes() {
        assert_eq!(truncate("一二三四五", 3), "一二三…");
        assert_eq!(truncate("一二三", 3), "一二三");
        assert_eq!(truncate("a  b\nc", 10), "a b c");
    }

    #[test]
    fn converts_time_to_beijing() {
        assert_eq!(fmt_time("2026-08-21T01:00:00Z").as_deref(), Some("08-21 09:00"));
        assert_eq!(fmt_time("not-a-time"), None);
    }

    #[test]
    fn falls_back_to_discovered_at_and_labels_it() {
        let mut item = sample_item();
        item.published_at = None;
        let meta = meta_line(&item).unwrap();
        assert!(meta.contains("收录"), "{}", meta);
        assert!(!meta.contains("发布"), "{}", meta);
    }

    #[test]
    fn renders_items_with_optional_fields_hidden() {
        let opts = RenderOptions {
            summary_max_chars: 100,
            show_reason: false,
            show_original_link: false,
        };
        let text = render_items("🤖 测试", &[sample_item()], &opts).to_text();
        assert!(text.contains("某模型发布"));
        assert!(text.contains("https://aihot.virxact.com/i/1"));
        assert!(!text.contains("值得关注的理由"));
        assert!(!text.contains("https://example.com/post"));
        assert!(text.contains("共 1 条"));
    }

    #[test]
    fn skips_null_summary_and_reason() {
        let mut item = sample_item();
        item.summary = None;
        item.reason = None;
        let opts = RenderOptions {
            summary_max_chars: 100,
            show_reason: true,
            show_original_link: true,
        };
        let text = render_items("🤖 测试", &[item], &opts).to_text();
        assert!(!text.contains("💡"));
        assert!(text.contains("📄 https://example.com/post"));
    }

    #[test]
    fn packs_entries_into_nodes_and_keeps_every_entry() {
        let opts = RenderOptions {
            summary_max_chars: 100,
            show_reason: false,
            show_original_link: false,
        };
        let items: Vec<Item> = (0..6).map(|_| sample_item()).collect();
        let rendered = render_items("🤖 测试", &items, &opts);

        let nodes = rendered.nodes(120);
        assert!(nodes.len() > 1, "超长内容应拆成多个节点");
        // 标题进首节点、落款独占末节点
        assert!(nodes[0].starts_with("🤖 测试"));
        assert_eq!(nodes.last().unwrap(), &rendered.footer);
        // 六条都在，一条不落
        let joined = nodes.join("\n");
        for idx in 1..=6 {
            assert!(joined.contains(&format!("{}. 某模型发布", idx)), "缺少第 {} 条", idx);
        }
    }

    #[test]
    fn plain_message_never_splits() {
        let rendered = Rendered::plain("📭 暂无资讯");
        assert_eq!(rendered.to_text(), "📭 暂无资讯");
        assert_eq!(rendered.nodes(10), vec!["📭 暂无资讯".to_string()]);
    }

    #[test]
    fn renders_models_and_omits_missing_fields() {
        use crate::plugins::ai_news::leaderboard::ModelEntry;

        let board = Board {
            updated_at: Some("8月21日 20:00".into()),
            source_count: Some(7),
            entries: vec![
                ModelEntry {
                    rank: Some(1),
                    rank_change: Some(1),
                    previous_rank: Some(2),
                    name: Some("Model A".into()),
                    provider: Some("Vendor".into()),
                    released_at: Some("2026-06-09T00:00:00.000Z".into()),
                    input_price_per_million_cny: Some(67.206),
                    output_price_per_million_cny: Some(336.03),
                    score: Some(89.4),
                    coverage: Some(0.88),
                    confidence: Some("HIGH".into()),
                    ..Default::default()
                },
                // 只有名字和分数：其余字段一概不显示，也不填 0
                ModelEntry {
                    rank: Some(2),
                    name: Some("Model B".into()),
                    score: Some(70.0),
                    ..Default::default()
                },
            ],
        };

        let text = render_models(&board, 12).to_text();
        assert!(text.contains("综合 7 家公开榜单 · 更新于 8月21日 20:00"));
        assert!(text.contains("第 1 名 Model A（Vendor） ↑1"));
        assert!(text.contains("共识分 89.4 · 完整度 88% · 可信度高"));
        assert!(text.contains("上线 2026-06-09 · 入 ¥67.21 / 出 ¥336"));
        assert!(text.contains("第 2 名 Model B"));
        assert!(text.contains(leaderboard::PAGE_URL));
        // 缺失字段不编造
        assert!(!text.contains("¥0"));
        assert!(!text.contains("完整度 0%"));
    }

    #[test]
    fn model_list_respects_the_display_cap() {
        use crate::plugins::ai_news::leaderboard::ModelEntry;

        let board = Board {
            entries: (1..=30)
                .map(|i| ModelEntry {
                    rank: Some(i),
                    name: Some(format!("Model {}", i)),
                    score: Some(50.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let rendered = render_models(&board, 5);
        assert_eq!(rendered.entries.len(), 5);
        assert!(rendered.entries[4].starts_with("第 5 名"));
    }

    #[test]
    fn renders_daily_within_block_budget() {
        let report = DailyReport {
            date: Some("2026-08-21".into()),
            title: None,
            lead: Some("今天的要点".into()),
            links: Links {
                aihot: Some("https://aihot.virxact.com/daily/2026-08-21".into()),
                ..Default::default()
            },
            sections: vec![DailyBlock {
                title: Some("模型".into()),
                text: None,
                url: None,
                children: vec![DailyBlock {
                    title: Some("条目一".into()),
                    text: Some("说明".into()),
                    url: Some("https://aihot.virxact.com/i/2".into()),
                    children: vec![],
                }],
            }],
            flashes: vec![],
        };
        let text = render_daily(&report, 8).to_text();
        assert!(text.contains("AI 日报 · 2026-08-21"));
        assert!(text.contains("今天的要点"));
        assert!(text.contains("条目一"));
        assert!(text.contains("完整日报："));
    }
}
