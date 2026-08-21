//! 把 AIHOT 的接口数据渲染成适合群聊阅读的纯文本。
//!
//! 原则：
//!   - 时间统一换算为北京时间（UTC+8）后再展示；
//!   - `publishedAt` 为空时回退 `discoveredAt`，并明确标注为「收录」，不冒充原文发布时间；
//!   - `summary` / `reason` 可能为 null，判空后再展示，绝不编造；
//!   - 主链接用站内页 `links.aihot`，仅在配置开启时附第三方原文；
//!   - 保持服务端返回顺序，不按 `score` 自行重排。

use super::api::{DailyBlock, DailyReport, HotTopic, Item, category_label};
use chrono::{DateTime, FixedOffset};

const DIVIDER: &str = "———————————————";

/// 北京时间（UTC+8）
fn beijing() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法时区偏移")
}

/// ISO8601 → `MM-DD HH:MM`（北京时间）
fn fmt_time(iso: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.with_timezone(&beijing()).format("%m-%d %H:%M").to_string())
}

/// 按字符（而非字节）截断，避免切坏中文
fn truncate(text: &str, max_chars: usize) -> String {
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
pub fn render_items(header: &str, items: &[Item], opts: &RenderOptions) -> String {
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    out.push_str(DIVIDER);
    out.push('\n');

    for (idx, item) in items.iter().enumerate() {
        let title = item.title.as_deref().unwrap_or("(无标题)").trim();
        out.push_str(&format!("{}. {}\n", idx + 1, title));

        if let Some(meta) = meta_line(item) {
            out.push_str(&format!("   {}\n", meta));
        }
        if let Some(summary) = item.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("   {}\n", truncate(summary, opts.summary_max_chars)));
        }
        if opts.show_reason
            && let Some(reason) = item.reason.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            out.push_str(&format!("   💡 {}\n", truncate(reason, opts.summary_max_chars)));
        }
        if let Some(link) = item.links.aihot.as_deref().filter(|s| !s.is_empty()) {
            out.push_str(&format!("   🔗 {}\n", link));
        }
        if opts.show_original_link
            && let Some(orig) = item.links.original.as_deref().filter(|s| !s.is_empty())
        {
            out.push_str(&format!("   📄 {}\n", orig));
        }
        out.push('\n');
    }

    out.push_str(DIVIDER);
    out.push_str(&format!("\n{} · 共 {} 条", super::api::ATTRIBUTION, items.len()));
    out
}

/// 热点榜：按 rank 展示「第 N 名」，不展示或推算热度值
pub fn render_hot_topics(topics: &[HotTopic]) -> String {
    let mut out = String::from("🔥 AI 当前热点榜\n");
    out.push_str(DIVIDER);
    out.push('\n');

    for (idx, topic) in topics.iter().enumerate() {
        let rank = topic.rank.unwrap_or((idx + 1) as u32);
        let title = topic.title.as_deref().unwrap_or("(无标题)").trim();
        out.push_str(&format!("第 {} 名 {}\n", rank, title));

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
            out.push_str(&format!("   {}\n", meta.join(" · ")));
        }

        if let Some(summary) = topic.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("   {}\n", truncate(summary, 80)));
        }
        if let Some(link) = topic.links.primary() {
            out.push_str(&format!("   🔗 {}\n", link));
        }
        out.push('\n');
    }

    out.push_str(DIVIDER);
    out.push_str(&format!("\n{}", super::api::ATTRIBUTION));
    out
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
pub fn render_daily(report: &DailyReport, max_blocks: usize) -> String {
    let mut out = String::new();
    let heading = match (report.date.as_deref(), report.title.as_deref()) {
        (Some(date), Some(title)) => format!("📰 AI 日报 · {}\n{}", date, title),
        (Some(date), None) => format!("📰 AI 日报 · {}", date),
        (None, Some(title)) => format!("📰 AI 日报 · {}", title),
        (None, None) => "📰 AI 日报".to_string(),
    };
    out.push_str(&heading);
    out.push('\n');
    out.push_str(DIVIDER);
    out.push('\n');

    if let Some(lead) = report.lead.as_deref() {
        out.push_str(&format!("{}\n\n", truncate(lead, 220)));
    }

    let mut budget = max_blocks.max(1);
    for section in &report.sections {
        if budget == 0 {
            break;
        }
        render_block(&mut out, section, 0, &mut budget);
        out.push('\n');
    }

    if budget > 0 && !report.flashes.is_empty() {
        out.push_str("⚡ 快讯\n");
        for flash in &report.flashes {
            if budget == 0 {
                break;
            }
            render_block(&mut out, flash, 1, &mut budget);
        }
        out.push('\n');
    }

    if let Some(link) = report.links.primary() {
        out.push_str(&format!("完整日报：{}\n", link));
    }
    out.push_str(DIVIDER);
    out.push_str(&format!("\n{}", super::api::ATTRIBUTION));
    out
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
        let text = render_items("🤖 测试", &[sample_item()], &opts);
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
        let text = render_items("🤖 测试", &[item], &opts);
        assert!(!text.contains("💡"));
        assert!(text.contains("📄 https://example.com/post"));
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
        let text = render_daily(&report, 8);
        assert!(text.contains("AI 日报 · 2026-08-21"));
        assert!(text.contains("今天的要点"));
        assert!(text.contains("条目一"));
        assert!(text.contains("完整日报："));
    }
}
