//! 主动推送任务：一次抓取，多群分发。
//!
//! 每个定时任务只向 AIHOT 发起一次请求，再把结果分发到各目标群，
//! 既满足官方「同一端点定时任务至少间隔 60 秒」的要求，也避免群数增长时把请求量放大。

use super::api::{self, Item};
use super::card;
use super::render::{self, RenderOptions, Rendered};
use super::state;
use super::{AiNewsConfig, LOG_TARGET};
use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::event::Context;
use crate::message::Message;
use std::time::Duration;

pub fn render_options(cfg: &AiNewsConfig) -> RenderOptions {
    RenderOptions {
        summary_max_chars: cfg.summary_max_chars.clamp(20, 400),
        show_reason: cfg.show_reason,
        show_original_link: cfg.show_original_link,
    }
}

/// 全局过滤：白名单模式下只发白名单群，否则跳过黑名单群
fn is_allowed(ctx: &Context, group_id: i64) -> bool {
    let guard = ctx.config.read().unwrap();
    let filter = &guard.global_filter;
    if filter.enable_whitelist {
        return filter.whitelist.contains(&group_id);
    }
    if filter.enable_blacklist {
        return !filter.blacklist.contains(&group_id);
    }
    true
}

/// 一次推送的成品：一张排版好的卡片图 + 一条带链接的文本。
///
/// 图片在进入分群循环前只截一次，多个群共用同一份 base64，
/// 免得每个群都去跑一遍无头浏览器。
pub struct Payload {
    pub rendered: Rendered,
    /// 卡片图的 base64；渲染失败或未开启时为 None，此时只发文本
    pub image: Option<String>,
}

impl Payload {
    /// 文本 + 卡片：卡片渲染失败不影响推送，退回纯文本继续
    pub async fn build(cfg: &AiNewsConfig, rendered: Rendered, card_html: Option<String>) -> Self {
        let image = match card_html {
            Some(html) if cfg.image_enabled => match card::capture(&html).await {
                Ok(b64) => Some(b64),
                Err(e) => {
                    warn!(target: LOG_TARGET, "卡片渲染失败，本次改发纯文本: {}", e);
                    None
                }
            },
            _ => None,
        };
        Self { rendered, image }
    }

    /// 只有文本，没有配图（错误提示、空结果等）
    pub fn text_only(rendered: Rendered) -> Self {
        Self {
            rendered,
            image: None,
        }
    }
}

/// 把渲染结果装配成待发消息。
///
/// 短内容照旧发一条纯文本；一旦整体超过 `forward_threshold_chars`，
/// 或者前面已经发过卡片图（此时文本只是链接附录），
/// 就按条目拆成合并转发的节点——群里只留一个折叠卡片，不刷屏。
/// 合并转发的消息链里只能放 node 段，因此这种情况下会舍弃回复引用。
pub fn build_message(
    ctx: &Context,
    cfg: &AiNewsConfig,
    rendered: &Rendered,
    reply_to: Option<i64>,
    force_forward: bool,
) -> Message {
    let threshold = cfg.forward_threshold_chars;
    let as_forward = threshold > 0
        && !rendered.entries.is_empty()
        && (force_forward || rendered.char_count() > threshold);

    if !as_forward {
        let msg = match reply_to {
            Some(id) => Message::new().reply(id),
            None => Message::new(),
        };
        return msg.text(rendered.to_text());
    }

    let bot_id = ctx.bot.login_user.id.parse::<i64>().unwrap_or(10000);
    let bot_name = ctx
        .bot
        .login_user
        .name
        .clone()
        .unwrap_or_else(|| "AI 资讯".to_string());

    let nodes = rendered.nodes(cfg.forward_node_chars);
    let mut forward = Message::new();
    for node in nodes {
        forward = forward.node_custom(bot_id, &bot_name, Message::new().text(node));
    }
    forward
}

/// 先图后文地投递一次推送：卡片图负责好看，随后的合并转发负责链接与检索。
///
/// 返回「对方是否收到了内容」——两条都没发出去才算失败，
/// 失败的条目不记入去重，留待下一轮补推。
pub async fn deliver(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    cfg: &AiNewsConfig,
    payload: &Payload,
    reply_to: Option<i64>,
) -> bool {
    let mut image_sent = false;

    if let Some(b64) = &payload.image {
        let mut msg = match reply_to {
            Some(id) => Message::new().reply(id),
            None => Message::new(),
        };
        msg = msg.image(format!("base64://{}", b64));

        match send_msg(ctx, writer.clone(), group_id, user_id, msg).await {
            Ok(_) => image_sent = true,
            Err(e) => warn!(target: LOG_TARGET, "卡片图发送失败，改由文本兜底: {}", e),
        }
    }

    // 图片已经带上了引用，文本就不必再引用一次
    let body = build_message(
        ctx,
        cfg,
        &payload.rendered,
        if image_sent { None } else { reply_to },
        image_sent,
    );
    let text_sent = match send_msg(ctx, writer, group_id, user_id, body).await {
        Ok(_) => true,
        Err(e) => {
            warn!(target: LOG_TARGET, "推送文本发送失败: {}", e);
            false
        }
    };

    image_sent || text_sent
}

/// 卡片图只画前 `image_max_items` 条，剩下的交给文本；图太长反而不好读
pub fn card_slice<'a, T>(items: &'a [T], cfg: &AiNewsConfig) -> &'a [T] {
    let max = cfg.image_max_items.clamp(1, 30);
    &items[..items.len().min(max)]
}

/// 目标群之间的间隔，防止短时间内连发触发风控
async fn pace(cfg: &AiNewsConfig) {
    tokio::time::sleep(Duration::from_secs(cfg.send_interval_seconds.clamp(1, 60))).await;
}

// ================= 抓取 =================

/// 拉取精选资讯。`conditional` 为 true 时启用 ETag 条件请求，
/// 返回 `Ok(None)` 表示服务端回了 304（内容没变），本轮无需推送。
pub async fn fetch_brief(
    cfg: &AiNewsConfig,
    conditional: bool,
) -> Result<Option<Vec<Item>>, api::ApiError> {
    let category = cfg.category.trim();
    api::fetch_items(
        &cfg.mode,
        &cfg.window,
        if category.is_empty() {
            None
        } else {
            Some(category)
        },
        None,
        cfg.limit,
        cfg.request_timeout_seconds,
        conditional,
    )
    .await
}

/// 关键词搜索：精选池查不到时，用完全相同的参数再查一次全量池。
/// 返回 (条目, 是否来自全量池)。
pub async fn search(
    cfg: &AiNewsConfig,
    query: &str,
) -> Result<(Vec<Item>, bool), api::ApiError> {
    let selected = api::fetch_items(
        "selected",
        "7d",
        None,
        Some(query),
        cfg.limit,
        cfg.request_timeout_seconds,
        false,
    )
    .await?
    .unwrap_or_default();

    if !selected.is_empty() {
        return Ok((selected, false));
    }

    let all = api::fetch_items(
        "all",
        "7d",
        None,
        Some(query),
        cfg.limit,
        cfg.request_timeout_seconds,
        false,
    )
    .await?
    .unwrap_or_default();

    Ok((all, true))
}

// ================= 定时推送 =================

/// 精选速递：只推该群没见过的条目
pub async fn push_brief(ctx: Context, writer: LockedWriter, cfg: AiNewsConfig, groups: Vec<i64>) {
    let items = match fetch_brief(&cfg, true).await {
        Ok(Some(items)) => items,
        Ok(None) => {
            info!(target: LOG_TARGET, "精选速递：服务端返回 304，无新内容，跳过。");
            return;
        }
        Err(e) => {
            error!(target: LOG_TARGET, "精选速递抓取失败: {}", e);
            return;
        }
    };

    if items.is_empty() {
        info!(target: LOG_TARGET, "精选速递：时间窗内没有条目，跳过。");
        return;
    }

    let keyed: Vec<(String, Item)> = items
        .into_iter()
        .filter_map(|item| item.dedupe_key().map(|key| (key, item)))
        .collect();
    let opts = render_options(&cfg);
    let subtitle = window_label(&cfg.window);

    for (idx, group_id) in groups.iter().enumerate() {
        if !is_allowed(&ctx, *group_id) {
            continue;
        }

        let keys: Vec<String> = keyed.iter().map(|(k, _)| k.clone()).collect();
        let fresh = state::unseen_keys(*group_id, keys, cfg.dedupe_days).await;

        if (fresh.len() as u32) < cfg.min_items.max(1) {
            info!(
                target: LOG_TARGET,
                "群 {} 新增条目 {} 条，低于阈值 {}，跳过。",
                group_id, fresh.len(), cfg.min_items.max(1)
            );
            continue;
        }

        let picked: Vec<Item> = keyed
            .iter()
            .filter(|(key, _)| fresh.contains(key))
            .map(|(_, item)| item.clone())
            .collect();

        // 每个群去重后的条目各不相同，卡片只能逐群渲染
        let header = format!("🤖 AI 资讯速递 · {}", subtitle);
        let rendered = render::render_items(&header, &picked, &opts);
        let card_html = Some(card::items_card(
            "AI 资讯速递",
            subtitle,
            card_slice(&picked, &cfg),
            &opts,
        ));
        let payload = Payload::build(&cfg, rendered, card_html).await;
        let sent = deliver(
            &ctx,
            writer.clone(),
            Some(*group_id),
            None,
            &cfg,
            &payload,
            None,
        )
        .await;

        // 只有真正发出去了才记入去重，发送失败的条目下次继续推
        if sent {
            state::mark_seen(*group_id, fresh).await;
        }

        if idx + 1 < groups.len() {
            pace(&cfg).await;
        }
    }
}

/// AI 日报：先查索引拿到实际日期，再取当期正文；同一期只推一次
pub async fn push_daily(ctx: Context, writer: LockedWriter, cfg: AiNewsConfig, groups: Vec<i64>) {
    let report = match api::fetch_latest_daily(cfg.request_timeout_seconds).await {
        Ok(Some(report)) => report,
        Ok(None) => {
            info!(target: LOG_TARGET, "AI 日报：当前没有可用日报，跳过。");
            return;
        }
        Err(e) => {
            error!(target: LOG_TARGET, "AI 日报抓取失败: {}", e);
            return;
        }
    };

    let Some(date) = report.date.clone() else {
        warn!(target: LOG_TARGET, "AI 日报缺少日期字段，跳过推送。");
        return;
    };
    // 各群内容一致，卡片只截一次
    let rendered = render::render_daily(&report, cfg.daily_max_blocks);
    let card_html = Some(card::daily_card(&report, cfg.daily_max_blocks));
    let payload = Payload::build(&cfg, rendered, card_html).await;

    for (idx, group_id) in groups.iter().enumerate() {
        if !is_allowed(&ctx, *group_id) {
            continue;
        }
        if state::has_pushed_daily(*group_id, &date).await {
            info!(target: LOG_TARGET, "群 {} 已推送过 {} 的日报，跳过。", group_id, date);
            continue;
        }

        if deliver(
            &ctx,
            writer.clone(),
            Some(*group_id),
            None,
            &cfg,
            &payload,
            None,
        )
        .await
        {
            state::mark_daily(*group_id, &date).await;
        }

        if idx + 1 < groups.len() {
            pace(&cfg).await;
        }
    }
}

/// 当前热点榜：快照式内容，不做条目去重
pub async fn push_hot_topics(
    ctx: Context,
    writer: LockedWriter,
    cfg: AiNewsConfig,
    groups: Vec<i64>,
) {
    let topics = match api::fetch_hot_topics(cfg.request_timeout_seconds, true).await {
        Ok(Some(topics)) => topics,
        Ok(None) => {
            info!(target: LOG_TARGET, "热点榜：服务端返回 304，无变化，跳过。");
            return;
        }
        Err(e) => {
            error!(target: LOG_TARGET, "热点榜抓取失败: {}", e);
            return;
        }
    };

    if topics.is_empty() {
        info!(target: LOG_TARGET, "热点榜：当前没有热点，跳过。");
        return;
    }

    let rendered = render::render_hot_topics(&topics);
    let card_html = Some(card::hot_topics_card(card_slice(&topics, &cfg)));
    let payload = Payload::build(&cfg, rendered, card_html).await;

    for (idx, group_id) in groups.iter().enumerate() {
        if !is_allowed(&ctx, *group_id) {
            continue;
        }
        let _ = deliver(
            &ctx,
            writer.clone(),
            Some(*group_id),
            None,
            &cfg,
            &payload,
            None,
        )
        .await;
        if idx + 1 < groups.len() {
            pace(&cfg).await;
        }
    }
}

pub fn window_label(window: &str) -> &'static str {
    match window {
        "24h" => "过去 24 小时",
        _ => "最近 7 天",
    }
}
