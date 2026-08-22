//! 把资讯排版成一张卡片图（HTML → 截图）。
//!
//! 设计取向：深色「夜读」版式，靠字号、字重与明度三级台阶拉开层级，
//! 而不是靠分割线堆砌。四类内容各有一个主色——速递靛蓝、热点橙、日报薄荷绿、
//! 模型榜琥珀金，主色只出现在序号、标签、顶部微光与引用左界这几处。
//!
//! 排版参数是按「群聊里被缩略图裹一层再点开看」这个真实场景定的：
//!   - 版心 720 CSS px 配合 `image_scale`（默认 3 倍）出图，2160px 宽，放大不糊；
//!   - 标题 26px、正文 18.5px——相对版心足够大，缩略图状态下也能读出标题；
//!   - 文字明度分三档（标题近白 / 正文 #BFC9D9 / 元信息 #8A94A8），
//!     一眼扫过先看到标题与数字，细节再往下沉；
//!   - 一条一格，格与格之间用 26px 上下留白而非重分割线。
//!
//! 图片只承载「读」的部分：链接一概不画进图里，改由随后的合并转发文本承载，
//! 这样图好看、链接又能点能搜。

use super::api::{DailyBlock, DailyReport, HotTopic, Item, category_label};
use super::leaderboard::{Board, Trend};
use super::render::{RenderOptions, fmt_time, truncate};
use anyhow::Result;
use cdp_html_shot::{Browser, CaptureOptions, Viewport};
use chrono::Utc;
use std::time::Duration;

/// 卡片主色。`rgb` 供 `rgba()` 调透明度用，避免写死多份色值。
#[derive(Clone, Copy)]
pub struct Accent {
    hex: &'static str,
    rgb: &'static str,
}

const BRIEF: Accent = Accent {
    hex: "#8296FF",
    rgb: "130,150,255",
};
const HOT: Accent = Accent {
    hex: "#FF8A5B",
    rgb: "255,138,91",
};
const DAILY: Accent = Accent {
    hex: "#3FD6AC",
    rgb: "63,214,172",
};
const MODELS: Accent = Accent {
    hex: "#FFC24B",
    rgb: "255,194,75",
};

/// 卡片渲染宽度（CSS 像素）。实际出图宽度 = WIDTH × `image_scale`（默认 3 倍 → 2160px）
const WIDTH: u32 = 720;

fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 出图时刻（北京时间），放在页眉右上角
fn stamp() -> String {
    Utc::now()
        .with_timezone(&super::render::beijing())
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

const CSS: &str = r#"
*{margin:0;padding:0;box-sizing:border-box}
.shot{padding:26px;background:#04060A}
.card{position:relative;overflow:hidden;border-radius:24px;padding:44px 44px 34px;
  background:#0C1017;border:1px solid rgba(255,255,255,.08);
  background-image:radial-gradient(rgba(255,255,255,.04) 1px,transparent 1px);
  background-size:28px 28px;
  font-family:"PingFang SC","Microsoft YaHei","Noto Sans CJK SC","Source Han Sans SC","WenQuanYi Zen Hei","Helvetica Neue",Arial,sans-serif;
  color:#F2F5FA;-webkit-font-smoothing:antialiased;text-rendering:geometricPrecision}
/* 左上角一团主色微光，给深底一点纵深，不喧宾夺主 */
.card::before{content:"";position:absolute;top:-230px;left:-130px;width:480px;height:480px;
  border-radius:50%;background:rgba(__RGB__,.20);filter:blur(95px);pointer-events:none}
.card>*{position:relative}

.eyebrow{display:flex;align-items:center;justify-content:space-between;margin-bottom:18px}
.kicker{display:flex;align-items:center;gap:10px;font-size:15px;font-weight:800;
  letter-spacing:.18em;color:__ACCENT__}
.dot{width:9px;height:9px;border-radius:50%;background:__ACCENT__;
  box-shadow:0 0 0 5px rgba(__RGB__,.16)}
.stamp{font-size:14px;color:#6B7689;letter-spacing:.04em;font-variant-numeric:tabular-nums}

.title{font-size:42px;line-height:1.24;font-weight:800;letter-spacing:-.015em;color:#FFFFFF}
.subtitle{margin-top:12px;font-size:18px;line-height:1.6;font-weight:500;color:#98A3B8}
.rule{height:2px;margin:28px 0 2px;border-radius:2px;
  background:linear-gradient(90deg,__ACCENT__,rgba(__RGB__,.35) 38%,rgba(255,255,255,.05))}

.row{display:grid;grid-template-columns:56px 1fr;gap:18px;padding:26px 0;
  border-bottom:1px solid rgba(255,255,255,.07)}
.row:last-child{border-bottom:none;padding-bottom:8px}
.idx{font-size:26px;font-weight:800;line-height:1.35;color:rgba(__RGB__,.85);
  font-variant-numeric:tabular-nums;text-align:right;letter-spacing:-.02em}
.rank{display:flex;align-items:center;justify-content:center;width:42px;height:42px;
  margin-left:auto;border-radius:12px;font-size:20px;font-weight:800;
  font-variant-numeric:tabular-nums;
  color:rgba(__RGB__,.95);background:rgba(__RGB__,.12);border:1px solid rgba(__RGB__,.28)}
.rank.top{color:#0A0D13;background:__ACCENT__;border-color:transparent;
  box-shadow:0 6px 18px rgba(__RGB__,.28)}

.h{font-size:26px;line-height:1.46;font-weight:700;letter-spacing:-.01em;color:#FFFFFF;
  display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden}
.meta{margin-top:12px;display:flex;flex-wrap:wrap;align-items:center;gap:10px;
  font-size:15px;font-weight:500;color:#8A94A8}
.sep{color:rgba(255,255,255,.16)}
.chip{padding:3px 11px;border-radius:7px;font-size:14px;font-weight:700;letter-spacing:.02em;
  color:__ACCENT__;background:rgba(__RGB__,.14)}
.chip.plain{color:#93A0B6;background:rgba(255,255,255,.07)}
.sum{margin-top:12px;font-size:18.5px;line-height:1.72;color:#BFC9D9;
  display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden}
.why{margin-top:14px;padding:12px 16px;border-left:4px solid __ACCENT__;
  border-radius:0 10px 10px 0;background:rgba(__RGB__,.09);
  font-size:17px;line-height:1.66;color:#CBD4E4;
  display:-webkit-box;-webkit-line-clamp:3;-webkit-box-orient:vertical;overflow:hidden}
.why b{color:__ACCENT__;font-weight:800;letter-spacing:.02em}

.lead{margin:26px 0 4px;padding:20px 22px;border-radius:14px;
  background:rgba(255,255,255,.05);border:1px solid rgba(255,255,255,.07);
  font-size:20px;line-height:1.74;font-weight:500;color:#D2DAE7}
.sec{padding:26px 0 6px;border-bottom:1px solid rgba(255,255,255,.07)}
.sec:last-of-type{border-bottom:none}
.sec-h{display:flex;align-items:center;gap:12px;font-size:22px;font-weight:800;color:#FFFFFF}
.bar{width:5px;height:22px;border-radius:3px;background:__ACCENT__}
.li{margin-top:16px;padding-left:20px;position:relative;font-size:19px;line-height:1.6;color:#C2CBDA}
.li::before{content:"";position:absolute;left:2px;top:12px;width:7px;height:7px;
  border-radius:50%;background:__ACCENT__}
.li b{color:#FFFFFF;font-weight:700}
.li .t{margin-top:6px;font-size:17px;line-height:1.68;color:#A5B0C4}

/* —— 模型榜 —— */
.mrow{display:grid;grid-template-columns:56px 1fr 150px;gap:18px;align-items:center;
  padding:22px 0;border-bottom:1px solid rgba(255,255,255,.07)}
.mrow:last-of-type{border-bottom:none}
.mname{display:flex;align-items:baseline;flex-wrap:wrap;gap:10px;
  font-size:25px;line-height:1.35;font-weight:700;color:#FFFFFF}
.trend{font-size:15px;font-weight:800;letter-spacing:.02em;
  font-variant-numeric:tabular-nums;color:#6B7689}
.trend.up{color:#4ADE80}
.trend.down{color:#FF7A7A}
.trend.new{color:__ACCENT__}
.meter{margin-top:12px;height:7px;border-radius:4px;background:rgba(255,255,255,.07);overflow:hidden}
.meter i{display:block;height:100%;border-radius:4px;
  background:linear-gradient(90deg,rgba(__RGB__,.55),__ACCENT__)}
.mscore{text-align:right}
.mscore strong{display:block;font-size:34px;line-height:1;font-weight:800;
  letter-spacing:-.02em;color:__ACCENT__;font-variant-numeric:tabular-nums}
.mscore small{display:block;margin-top:6px;font-size:14px;line-height:1.4;color:#8A94A8;white-space:nowrap}
.note{margin-top:22px;font-size:15px;line-height:1.7;color:#7E8899}

.foot{display:flex;align-items:center;justify-content:space-between;
  margin-top:30px;padding-top:20px;border-top:1px solid rgba(255,255,255,.09);
  font-size:14px;font-weight:500;color:#6B7689;letter-spacing:.02em}
.foot .src{display:flex;align-items:center;gap:10px}
.mark{width:5px;height:5px;border-radius:50%;background:rgba(__RGB__,.7);
  box-shadow:9px 0 0 rgba(255,255,255,.14),18px 0 0 rgba(255,255,255,.08)}
"#;

/// 套上统一的卡片外壳：页眉（主色标签 + 出图时间）、大标题、正文、页脚。
///
/// `foot_note` 是页脚右侧那句提示。它对应的是随后真的会发出去的那条消息，
/// 别写成图里说一套、群里发另一套。
fn shell(
    accent: Accent,
    kicker: &str,
    title: &str,
    subtitle: &str,
    body: &str,
    foot_note: &str,
) -> String {
    let css = CSS.replace("__ACCENT__", accent.hex).replace("__RGB__", accent.rgb);
    let subtitle = if subtitle.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="subtitle">{}</div>"#, esc(subtitle))
    };

    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8"><style>{css}</style></head>
<body><div class="shot"><div class="card">
<div class="eyebrow"><div class="kicker"><span class="dot"></span>{kicker}</div><div class="stamp">{stamp}</div></div>
<div class="title">{title}</div>{subtitle}
<div class="rule"></div>
{body}
<div class="foot"><div class="src"><span class="mark"></span>AIHOT · aihot.virxact.com</div><div>{foot_note}</div></div>
</div></div></body></html>"#,
        css = css,
        kicker = esc(kicker),
        stamp = stamp(),
        title = esc(title),
        subtitle = subtitle,
        body = body,
        foot_note = esc(foot_note),
    )
}

/// 图后必定跟一条带链接的文本，页脚据此措辞
const FOOT_LINKS: &str = "完整链接见下方合并转发";

/// 一条资讯的元信息行：来源 · 分类 · 时间
fn meta_html(item: &Item) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(name) = item
        .source
        .as_ref()
        .and_then(|s| s.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(format!(r#"<span>{}</span>"#, esc(name)));
    }
    if let Some(cat) = item.category.as_deref().filter(|c| !c.trim().is_empty()) {
        parts.push(format!(r#"<span class="chip">{}</span>"#, esc(category_label(cat))));
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
                .map(|t| format!("{} 收录", t))
        });
    if let Some(t) = time {
        parts.push(format!(r#"<span>{}</span>"#, esc(&t)));
    }

    if parts.is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="meta">{}</div>"#,
        parts.join(r#"<span class="sep">·</span>"#)
    )
}

/// 资讯列表卡片（速递 / 搜索结果共用）
pub fn items_card(title: &str, subtitle: &str, items: &[Item], opts: &RenderOptions) -> String {
    let mut body = String::new();

    for (idx, item) in items.iter().enumerate() {
        body.push_str(&format!(
            r#"<div class="row"><div class="idx">{:02}</div><div>"#,
            idx + 1
        ));
        body.push_str(&format!(
            r#"<div class="h">{}</div>"#,
            esc(item.title.as_deref().unwrap_or("(无标题)").trim())
        ));
        body.push_str(&meta_html(item));

        if let Some(summary) = item.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!(
                r#"<div class="sum">{}</div>"#,
                esc(&truncate(summary, 100))
            ));
        }
        if opts.show_reason
            && let Some(reason) = item.reason.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            // 给「推荐理由」一个主色小标签：这是全卡最该被先看见的一句
            body.push_str(&format!(
                r#"<div class="why"><b>推荐理由</b> {}</div>"#,
                esc(&truncate(reason, 80))
            ));
        }
        body.push_str("</div></div>");
    }

    let subtitle = match subtitle.is_empty() {
        true => format!("共 {} 条", items.len()),
        false => format!("{} · 共 {} 条", subtitle, items.len()),
    };
    shell(BRIEF, "AI NEWS", title, &subtitle, &body, FOOT_LINKS)
}

/// 热点榜卡片：前三名用实心序号牌，其余描边，一眼看出梯队
pub fn hot_topics_card(topics: &[HotTopic]) -> String {
    let mut body = String::new();

    for (idx, topic) in topics.iter().enumerate() {
        let rank = topic.rank.unwrap_or((idx + 1) as u32);
        let cls = if rank <= 3 { "rank top" } else { "rank" };

        body.push_str(&format!(
            r#"<div class="row"><div class="idx"><div class="{}">{}</div></div><div>"#,
            cls, rank
        ));
        body.push_str(&format!(
            r#"<div class="h">{}</div>"#,
            esc(topic.title.as_deref().unwrap_or("(无标题)").trim())
        ));

        let mut meta: Vec<String> = Vec::new();
        if let Some(count) = topic.source_count.filter(|c| *c > 0) {
            meta.push(format!(r#"<span class="chip">{} 个信源</span>"#, count));
        }
        let names: Vec<String> = topic
            .source_names
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .take(3)
            .map(|s| format!(r#"<span class="chip plain">{}</span>"#, esc(s)))
            .collect();
        meta.extend(names);
        if let Some(t) = topic.latest_at.as_deref().and_then(fmt_time) {
            meta.push(format!(r#"<span>最新 {}</span>"#, esc(&t)));
        }
        if !meta.is_empty() {
            body.push_str(&format!(r#"<div class="meta">{}</div>"#, meta.join("")));
        }

        if let Some(summary) = topic.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!(
                r#"<div class="sum">{}</div>"#,
                esc(&truncate(summary, 100))
            ));
        }
        body.push_str("</div></div>");
    }

    let subtitle = format!("跨信源聚合 · TOP {}", topics.len());
    shell(HOT, "AI HOTLIST", "当前热点榜", &subtitle, &body, FOOT_LINKS)
}

/// 模型榜卡片：一行一个模型，左名次、中模型与价格、右共识分。
///
/// 共识分是这张图唯一需要「一眼看到」的数字，所以给到 34px 主色 + 一条同色进度条；
/// 其余信息（厂商、上线日期、价格）压到 15px 的灰阶里，不与之争。
pub fn models_card(board: &Board, max_items: usize) -> String {
    let shown = &board.entries[..board.entries.len().min(max_items.max(1))];
    let mut body = String::new();

    for (idx, model) in shown.iter().enumerate() {
        let rank = model.rank.unwrap_or((idx + 1) as u32);
        let rank_cls = if rank <= 3 { "rank top" } else { "rank" };

        body.push_str(&format!(
            r#"<div class="mrow"><div><div class="{}">{}</div></div><div>"#,
            rank_cls, rank
        ));

        let trend = model.trend();
        let trend_html = match trend {
            Trend::Flat => String::new(),
            Trend::Up(_) => format!(r#"<span class="trend up">{}</span>"#, esc(&trend.marker())),
            Trend::Down(_) => format!(r#"<span class="trend down">{}</span>"#, esc(&trend.marker())),
            Trend::New => format!(r#"<span class="trend new">{}</span>"#, esc(&trend.marker())),
        };
        body.push_str(&format!(
            r#"<div class="mname">{}{}</div>"#,
            esc(model.display_name()),
            trend_html
        ));

        let mut meta: Vec<String> = Vec::new();
        if let Some(provider) = model.provider_name() {
            meta.push(format!(r#"<span class="chip">{}</span>"#, esc(provider)));
        }
        if let Some(date) = model.released_date() {
            meta.push(format!(r#"<span>上线 {}</span>"#, esc(date)));
        }
        if let Some(ctx_len) = model.context_text() {
            meta.push(format!(r#"<span>上下文 {}</span>"#, esc(&ctx_len)));
        }
        if let Some(price) = model.price_text() {
            meta.push(format!(r#"<span>{}</span>"#, esc(&price)));
        }
        if !meta.is_empty() {
            body.push_str(&format!(r#"<div class="meta">{}</div>"#, meta.join("")));
        }

        // 分数条按 0—100 直接映射，长度即分数，不做二次拉伸
        if let Some(score) = model.score {
            body.push_str(&format!(
                r#"<div class="meter"><i style="width:{:.1}%"></i></div>"#,
                score.clamp(0.0, 100.0)
            ));
        }
        body.push_str("</div>");

        // 完整度与可信度各占一行：右栏窄，挤在一行会在「可信度」和「高」之间折行
        let mut note = String::new();
        if let Some(coverage) = model.coverage_text() {
            note.push_str(&format!("<small>完整度 {}</small>", esc(&coverage)));
        }
        if let Some(level) = model.confidence_label() {
            note.push_str(&format!("<small>可信度 {}</small>", level));
        }
        body.push_str(&format!(
            r#"<div class="mscore"><strong>{}</strong>{}</div></div>"#,
            esc(&model.score_text().unwrap_or_else(|| "—".to_string())),
            note
        ));
    }

    body.push_str(
        r#"<div class="note">共识分由多家公开评测榜单统一折算，只反映公开评测的汇总结果；价格为厂商官网参考价，人民币／百万 Token。</div>"#,
    );

    let mut subtitle: Vec<String> = Vec::new();
    if let Some(count) = board.source_count {
        subtitle.push(format!("综合 {} 家公开榜单", count));
    }
    if let Some(updated) = board.updated_at.as_deref().filter(|s| !s.is_empty()) {
        subtitle.push(format!("更新于 {}", updated));
    }
    subtitle.push(format!("TOP {}", shown.len()));

    shell(
        MODELS,
        "MODEL CONSENSUS",
        "AIHOT 大模型排行榜",
        &subtitle.join(" · "),
        &body,
        "完整榜单见下方链接",
    )
}

/// 日报里的一个条目（含其子条目），扁平成带圆点的列表行
fn daily_items(out: &mut String, block: &DailyBlock, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let title = block.title.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let text = block.text.as_deref().map(str::trim).filter(|s| !s.is_empty());

    if title.is_some() || text.is_some() {
        out.push_str(r#"<div class="li">"#);
        if let Some(t) = title {
            out.push_str(&format!("<b>{}</b>", esc(t)));
        }
        if let Some(t) = text {
            let cls = if title.is_some() { r#" class="t""# } else { "" };
            out.push_str(&format!("<div{}>{}</div>", cls, esc(&truncate(t, 150))));
        }
        out.push_str("</div>");
        *budget -= 1;
    }

    for child in &block.children {
        daily_items(out, child, budget);
    }
}

/// AI 日报卡片：保留 lead / sections / flashes 的分栏结构
pub fn daily_card(report: &DailyReport, max_blocks: usize) -> String {
    let mut body = String::new();

    if let Some(lead) = report.lead.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str(&format!(
            r#"<div class="lead">{}</div>"#,
            esc(&truncate(lead, 260))
        ));
    }

    let mut budget = max_blocks.max(1);
    for section in &report.sections {
        if budget == 0 {
            break;
        }
        let mut items = String::new();
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

        body.push_str(r#"<div class="sec">"#);
        if let Some(t) = section.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!(
                r#"<div class="sec-h"><span class="bar"></span>{}</div>"#,
                esc(t)
            ));
        }
        body.push_str(&items);
        body.push_str("</div>");
    }

    if budget > 0 && !report.flashes.is_empty() {
        let mut items = String::new();
        for flash in &report.flashes {
            daily_items(&mut items, flash, &mut budget);
        }
        if !items.is_empty() {
            body.push_str(
                r#"<div class="sec"><div class="sec-h"><span class="bar"></span>快讯</div>"#,
            );
            body.push_str(&items);
            body.push_str("</div>");
        }
    }

    let title = match report.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => t.to_string(),
        None => "AI 日报".to_string(),
    };
    let subtitle = report.date.as_deref().unwrap_or_default();

    shell(DAILY, "AI DAILY", &title, subtitle, &body, FOOT_LINKS)
}

/// 把卡片 HTML 截成图，返回 base64（PNG/JPEG 由截图端决定）。
///
/// 先按固定宽度定版，量出实际高度后再放大视口重截，
/// 保证长卡片一次出完，不会被视口截断。
///
/// `scale` 是设备像素比：版心宽度不变、出图分辨率翻倍，字形边缘更实，
/// 群里放大看不至于发虚。取值过大只会把图撑肥，这里限制在 1—4 倍。
pub async fn capture(html: &str, scale: f64) -> Result<String> {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        3.0
    };
    let browser = Browser::instance().await;
    let tab = browser.new_tab().await.map_err(|e| anyhow::anyhow!(e))?;

    let result = async {
        tab.set_viewport(&Viewport::new(WIDTH, 600).with_device_scale_factor(scale))
            .await?;
        tab.set_content(html).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;

        let height = tab
            .evaluate("document.body.scrollHeight")
            .await?
            .as_f64()
            .unwrap_or(1200.0) as u32;
        let viewport =
            Viewport::new(WIDTH, (height + 40).clamp(400, 12000)).with_device_scale_factor(scale);
        tab.set_viewport(&viewport).await?;
        tokio::time::sleep(Duration::from_millis(80)).await;

        let opts = CaptureOptions::new().with_viewport(viewport).with_quality(92);
        let b64 = tab.find_element(".shot").await?.screenshot_with_options(opts).await?;
        Ok::<String, anyhow::Error>(b64)
    }
    .await;

    let _ = tab.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把三种卡片的 HTML 落到 `AI_NEWS_CARD_DUMP` 指定的目录，方便肉眼校版：
    ///   AI_NEWS_CARD_DUMP=/tmp/cards cargo test card::tests::dump -- --ignored
    #[test]
    #[ignore = "仅用于人工核对排版"]
    fn dump_sample_cards() {
        use crate::plugins::ai_news::api::{DailyBlock, Links, Source};

        let Ok(dir) = std::env::var("AI_NEWS_CARD_DUMP") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();

        let opts = RenderOptions {
            summary_max_chars: 90,
            show_reason: true,
            show_original_link: false,
        };
        let titles = [
            ("Anthropic 发布 Claude 新一代模型，长上下文推理能力显著提升", "Anthropic 官方博客", "ai-models"),
            ("OpenAI 开放 Realtime API 正式版，延迟降至 300ms 以内", "OpenAI Blog", "ai-products"),
            ("研究显示大模型在多步数学推理中仍依赖模式匹配而非符号演算", "arXiv", "paper"),
            ("英伟达下季度数据中心营收指引超预期，AI 芯片需求未见放缓", "路透社", "industry"),
            ("实践总结：用结构化输出把 LLM 接入既有业务系统的六个要点", "少数派", "tip"),
        ];
        let items: Vec<Item> = titles
            .iter()
            .map(|(t, src, cat)| Item {
                id: Some((*t).into()),
                title: Some((*t).into()),
                summary: Some("模型在数学、代码与长文档理解等基准上取得明显提升，官方同时公布了新的定价方案与迁移指南，开发者可即刻通过 API 调用。".into()),
                reason: Some("发布节奏与竞品形成直接对位，对下游应用的选型有实际影响。".into()),
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

        for (name, html) in [
            ("brief.html", items_card("AI 资讯速递", "过去 24 小时", &items, &opts)),
            ("hot.html", hot_topics_card(&topics)),
            ("daily.html", daily_card(&report, 12)),
            ("models.html", models_card(&board, 12)),
        ] {
            std::fs::write(format!("{}/{}", dir, name), html).unwrap();
        }
    }

    /// 端到端跑一次截图，确认 HTML 能被无头浏览器吃下并出图：
    ///   AI_NEWS_CARD_DUMP=/tmp/cards cargo test card::tests::captures -- --ignored
    #[tokio::test]
    #[ignore = "需要可用的无头浏览器"]
    async fn captures_a_card_to_png() {
        let Ok(dir) = std::env::var("AI_NEWS_CARD_DUMP") else {
            return;
        };
        dump_sample_cards();
        let html = std::fs::read_to_string(format!("{}/brief.html", dir)).unwrap();

        let b64 = capture(&html, 3.0).await.expect("截图应当成功");
        assert!(!b64.is_empty());

        use base64::{Engine, engine::general_purpose::STANDARD};
        let bytes = STANDARD.decode(&b64).expect("截图应是合法 base64");
        std::fs::write(format!("{}/captured.png", dir), &bytes).unwrap();
        println!("出图 {} 字节", bytes.len());
    }

    #[test]
    fn escapes_html_in_external_content() {
        let html = esc(r#"<script>alert("x")</script>"#);
        assert!(!html.contains('<'));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn items_card_embeds_titles_and_drops_links() {
        use crate::plugins::ai_news::api::{Links, Source};
        let item = Item {
            id: Some("1".into()),
            title: Some("某模型发布".into()),
            summary: Some("摘要".into()),
            reason: None,
            source: Some(Source {
                name: Some("官方博客".into()),
            }),
            links: Links {
                aihot: Some("https://aihot.virxact.com/i/1".into()),
                ..Default::default()
            },
            published_at: Some("2026-08-21T01:00:00Z".into()),
            discovered_at: None,
            category: Some("ai-models".into()),
        };
        let opts = RenderOptions {
            summary_max_chars: 100,
            show_reason: true,
            show_original_link: false,
        };
        let html = items_card("过去 24 小时", "", &[item], &opts);

        assert!(html.contains("某模型发布"));
        assert!(html.contains("官方博客"));
        assert!(html.contains("08-21 09:00"), "时间应换算为北京时间");
        // 链接只走文本消息，不画进图里
        assert!(!html.contains("aihot.virxact.com/i/1"));
    }
}
