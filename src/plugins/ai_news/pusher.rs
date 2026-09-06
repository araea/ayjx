//! 主动推送任务：一次抓取，多会话分发。
//!
//! 每个定时任务只向 AIHOT 发起一次请求，再把结果分发到各目标会话，
//! 既满足官方「同一端点定时任务至少间隔 60 秒」的要求，也避免目标数增长时放大请求量。

use super::api::{self, Item};
use super::card;
use super::render::{self, RenderOptions, Rendered};
use super::state;
use super::{AiNewsConfig, LOG_TARGET, PushTarget};
use crate::adapters::satori::{BotError, LockedWriter, send_msg, send_msg_id};
use crate::event::Context;
use crate::message::Message;
use rand::RngExt;
use std::time::Duration;

/// 两条推送线的数据源是产品语义，不跟随手动查询的通用 `mode` 配置：
/// 定时线固定精选；实时线默认精选，也可显式切到全量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushFeed {
    Realtime,
    Curated,
}

impl PushFeed {
    fn mode(self) -> &'static str {
        match self {
            Self::Realtime => "all",
            Self::Curated => "selected",
        }
    }

    fn request_limit(
        self,
        cfg: &AiNewsConfig,
        has_group_override: bool,
        realtime: bool,
    ) -> u32 {
        if realtime {
            // 实时抓取固定取接口上限，再交给持久队列分批发送。这里若沿用展示
            // 条数，突发时排在 limit 之后的条目会因下一轮 304 而永久漏掉。
            return 100;
        }
        match self {
            Self::Realtime => 100,
            Self::Curated if has_group_override => cfg.limit.saturating_mul(5).clamp(1, 100),
            Self::Curated => cfg.limit.clamp(1, 100),
        }
    }
}

pub fn render_options(cfg: &AiNewsConfig) -> RenderOptions {
    RenderOptions {
        summary_max_chars: cfg.summary_max_chars.clamp(20, 400),
        show_reason: cfg.show_reason,
        show_original_link: cfg.show_original_link,
    }
}

fn card_theme(cfg: &AiNewsConfig) -> card::CardTheme {
    card::resolve_theme(&cfg.card_theme)
}

/// 全局频道过滤只约束群聊；显式加入的私聊目标不属于频道黑白名单。
pub(super) fn is_allowed(ctx: &Context, target: PushTarget) -> bool {
    let PushTarget::Group(group_id) = target else {
        return true;
    };
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

/// 一次推送的成品：一张排版好的卡片图，以及仅在用户引用提取时发送的文本。
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
            Some(html) if cfg.image_enabled => match card::capture(&html, cfg.image_scale).await {
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

/// 把渲染结果装配成待发文本消息（截图失败兜底、或按需提取时使用）。
///
/// 短内容照旧发一条纯文本；一旦整体超过 `forward_threshold_chars`，
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

fn retryable_pre_send_error(error: &str) -> bool {
    // 这些错误都发生在 Satori 进入 QQ sendMsg 之前，重试不会重复发送。
    error.contains("QQ kernel offline or not ready")
        || error.contains("QQ session stabilizing")
        || error.contains("outbound queue full")
        || error.contains("outbound queue timeout")
        || error.contains("outbound circuit open")
        || error.contains("outbound rate budget exhausted")
}

async fn send_card_with_recovery(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
    user_id: Option<i64>,
    b64: &str,
    reply_to: Option<i64>,
) -> Result<Option<String>, BotError> {
    let mut last_error = None;
    for attempt in 0..3 {
        let mut msg = match reply_to {
            Some(id) => Message::new().reply(id),
            None => Message::new(),
        };
        msg = msg.image(format!("base64://{b64}"));

        match send_msg_id(ctx, writer.clone(), group_id, user_id, msg).await {
            Ok(id) => return Ok(id),
            Err(error) => {
                let detail = error.to_string();
                if attempt == 2 || !retryable_pre_send_error(&detail) {
                    return Err(error);
                }
                let delay = if detail.contains("session stabilizing") {
                    32
                } else {
                    5
                };
                warn!(
                    target: LOG_TARGET,
                    "卡片发送遇到可恢复的 Satori 状态（第 {}/3 次）: {}；{} 秒后重试",
                    attempt + 1,
                    detail,
                    delay
                );
                last_error = Some(detail);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| "卡片发送重试耗尽".to_string())
        .into())
}

/// 投递一次推送。卡片成功发送时一级内容只有图片，并保存图片消息 ID 与文本的
/// 映射；用户之后引用该图执行提取指令时，才发送正文和链接。
///
/// 卡片渲染/发送失败，或实现端没有返回可关联的消息 ID 时，退回纯文本，避免
/// 用户看到一张无法提取链接的孤立图片。
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
    let mut extraction_saved = false;

    if let Some(b64) = &payload.image {
        match send_card_with_recovery(ctx, writer.clone(), group_id, user_id, b64, reply_to).await {
            Ok(Some(message_id)) => {
                image_sent = true;
                let target_id = group_id.or_else(|| user_id.map(|id| -id)).unwrap_or_default();
                state::remember_extraction(target_id, message_id, payload.rendered.clone()).await;
                extraction_saved = true;
            }
            Ok(None) => {
                image_sent = true;
                warn!(target: LOG_TARGET, "卡片图未返回消息 ID，无法关联引用提取，改由文本兜底。")
            }
            Err(e) => warn!(target: LOG_TARGET, "卡片图发送失败，改由文本兜底: {}", e),
        }
    }

    if extraction_saved {
        return true;
    }

    // 图片若已发出但无法登记映射，兜底文本不再重复引用原指令。
    let body = build_message(
        ctx,
        cfg,
        &payload.rendered,
        if image_sent { None } else { reply_to },
        false,
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

/// 一次推送的标题组：文本消息的头一行，以及卡片图的主副标题
pub(super) struct Headline<'a> {
    pub text: &'a str,
    pub card_title: &'a str,
    pub card_subtitle: &'a str,
}

/// 把一批资讯渲染成卡片图并投递给某一个群。
///
/// 定时速递与实时快报都走这里：两者的差别只在标题与挑选条目的规则，
/// 排版、截图和引用提取的投递逻辑完全一致。
///
/// 每个群去重后的条目各不相同，卡片只能逐群渲染，无法像日报那样共用一张图。
pub(super) async fn deliver_items(
    ctx: &Context,
    writer: LockedWriter,
    target: PushTarget,
    cfg: &AiNewsConfig,
    headline: Headline<'_>,
    items: &[Item],
) -> bool {
    let opts = render_options(cfg);
    let rendered = render::render_items(headline.text, items, &opts);
    let card_html = card::items_card(
        headline.card_title,
        headline.card_subtitle,
        card_slice(items, cfg),
        &opts,
        card_theme(cfg),
    );
    let payload = Payload::build(cfg, rendered, Some(card_html)).await;

    deliver(
        ctx,
        writer,
        target.group_id(),
        target.user_id(),
        cfg,
        &payload,
        None,
    )
    .await
}

/// 卡片图只画前 `image_max_items` 条；其余条目仍保留在“提取全部”的文本中。
pub fn card_slice<'a, T>(items: &'a [T], cfg: &AiNewsConfig) -> &'a [T] {
    let max = cfg.image_max_items.clamp(1, 30);
    &items[..items.len().min(max)]
}

/// 目标群之间的间隔：在配置的 min—max 之间随机取值。
///
/// 用随机而非固定间隔，一是不让所有群在同一秒收到同一张图，
/// 二是避免整齐的节拍撞上风控阈值。
pub(super) async fn pace(cfg: &AiNewsConfig) {
    tokio::time::sleep(Duration::from_secs(pace_seconds(cfg))).await;
}

fn pace_seconds(cfg: &AiNewsConfig) -> u64 {
    let min = cfg.send_interval_seconds.clamp(1, 600);
    let max = cfg.send_interval_max_seconds.clamp(min, 600);
    if min == max {
        return min;
    }
    rand::rng().random_range(min..=max)
}

// ================= 抓取 =================

/// 拉取精选资讯。`poll` 为 [`api::Poll::Cached`] 时启用 ETag 条件请求，
/// 返回 `Ok(None)` 表示服务端回了 304（内容没变），本轮无需推送。
pub async fn fetch_brief(
    cfg: &AiNewsConfig,
    poll: api::Poll,
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
        poll,
    )
    .await
}

/// 主动推送需要兼顾按目标分类覆盖：只要有目标设置了独立分类，就抓取未筛选的
/// 动态池，再在本地按目标过滤；仍然只发起一次请求，不随目标数放大流量。
async fn fetch_feed_for_push(
    cfg: &AiNewsConfig,
    poll: api::Poll,
    feed: PushFeed,
    realtime: bool,
) -> Result<Option<Vec<Item>>, api::ApiError> {
    let has_group_override = cfg
        .group_preferences
        .values()
        .any(|preference| preference.category.is_some());
    let category = cfg.category.trim();

    api::fetch_items(
        feed.mode(),
        &cfg.window,
        if has_group_override || category.is_empty() {
            None
        } else {
            Some(category)
        },
        None,
        feed.request_limit(cfg, has_group_override, realtime),
        cfg.request_timeout_seconds,
        poll,
    )
    .await
}

/// 定时速递固定读取精选池；通用 `mode` 配置只影响手动查询。
pub async fn fetch_brief_for_push(
    cfg: &AiNewsConfig,
    poll: api::Poll,
) -> Result<Option<Vec<Item>>, api::ApiError> {
    fetch_feed_for_push(cfg, poll, PushFeed::Curated, false).await
}

/// 实时快报默认读取精选动态；显式配置为 `all` 时读取全部公开动态。
/// 两种模式都以接口上限抓取，避免突发资讯遗漏。
pub async fn fetch_realtime_for_push(
    cfg: &AiNewsConfig,
    poll: api::Poll,
) -> Result<Option<Vec<Item>>, api::ApiError> {
    let feed = if cfg.realtime_uses_all() {
        PushFeed::Realtime
    } else {
        PushFeed::Curated
    };
    fetch_feed_for_push(cfg, poll, feed, true).await
}

pub(super) fn item_matches_target(cfg: &AiNewsConfig, target: PushTarget, item: &Item) -> bool {
    let category = cfg.category_for_target(target);
    category.is_empty() || item.category.as_deref() == Some(category)
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
        api::Poll::Fresh,
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
        api::Poll::Fresh,
    )
    .await?
    .unwrap_or_default();

    Ok((all, true))
}

// ================= 定时推送 =================

/// 精选速递：只推该目标没见过的条目
pub async fn push_brief(
    ctx: Context,
    writer: LockedWriter,
    cfg: AiNewsConfig,
    targets: Vec<PushTarget>,
) {
    let items = match fetch_brief_for_push(&cfg, api::Poll::Cached("brief")).await {
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
    let subtitle = window_label(&cfg.window);
    let header = format!("🤖 AI 资讯速递 · {}", subtitle);

    let mut attempted_any = false;
    for target in targets {
        if !is_allowed(&ctx, target) {
            continue;
        }

        let group_items: Vec<&(String, Item)> = keyed
            .iter()
            .filter(|(_, item)| item_matches_target(&cfg, target, item))
            .take(cfg.limit.clamp(1, 100) as usize)
            .collect();
        let keys: Vec<String> = group_items.iter().map(|(key, _)| key.clone()).collect();
        let fresh = state::unseen_brief_keys(target.state_id(), keys, cfg.dedupe_days).await;

        if (fresh.len() as u32) < cfg.min_items.max(1) {
            info!(
                target: LOG_TARGET,
                "{} 新增条目 {} 条，低于阈值 {}，跳过。",
                target, fresh.len(), cfg.min_items.max(1)
            );
            continue;
        }

        let picked: Vec<Item> = group_items
            .iter()
            .filter(|(key, _)| fresh.contains(key))
            .map(|(_, item)| (*item).clone())
            .collect();

        if attempted_any {
            pace(&cfg).await;
        }
        attempted_any = true;

        let sent = deliver_items(
            &ctx,
            writer.clone(),
            target,
            &cfg,
            Headline {
                text: &header,
                card_title: "AI 资讯速递",
                card_subtitle: subtitle,
            },
            &picked,
        )
        .await;

        // 只有真正发出去了才记入去重，发送失败的条目下次继续推
        if sent {
            state::mark_brief_seen(target.state_id(), fresh).await;
        }

    }
}

/// AI 日报：先查索引拿到实际日期，再取当期正文；同一期只推一次
pub async fn push_daily(
    ctx: Context,
    writer: LockedWriter,
    cfg: AiNewsConfig,
    targets: Vec<PushTarget>,
) {
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
    let card_html = Some(card::daily_card(
        &report,
        cfg.daily_max_blocks,
        card_theme(&cfg),
    ));
    let payload = Payload::build(&cfg, rendered, card_html).await;

    let mut attempted_any = false;
    for target in targets {
        if !is_allowed(&ctx, target) {
            continue;
        }
        if state::has_pushed_daily(target.state_id(), &date).await {
            info!(target: LOG_TARGET, "{} 已推送过 {} 的日报，跳过。", target, date);
            continue;
        }

        if attempted_any {
            pace(&cfg).await;
        }
        attempted_any = true;

        if deliver(
            &ctx,
            writer.clone(),
            target.group_id(),
            target.user_id(),
            &cfg,
            &payload,
            None,
        )
        .await
        {
            state::mark_daily(target.state_id(), &date).await;
        }

    }
}

/// 当前热点榜：快照式内容，不做条目去重
pub async fn push_hot_topics(
    ctx: Context,
    writer: LockedWriter,
    cfg: AiNewsConfig,
    targets: Vec<PushTarget>,
) {
    let poll = api::Poll::Cached("hot");
    let topics = match api::fetch_hot_topics(cfg.request_timeout_seconds, poll).await {
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
    let card_html = Some(card::hot_topics_card(
        card_slice(&topics, &cfg),
        card_theme(&cfg),
    ));
    let payload = Payload::build(&cfg, rendered, card_html).await;

    let mut attempted_any = false;
    for target in targets {
        if !is_allowed(&ctx, target) {
            continue;
        }
        if attempted_any {
            pace(&cfg).await;
        }
        attempted_any = true;
        let _ = deliver(
            &ctx,
            writer.clone(),
            target.group_id(),
            target.user_id(),
            &cfg,
            &payload,
            None,
        )
        .await;
    }
}

pub fn window_label(window: &str) -> &'static str {
    match window {
        "24h" => "过去 24 小时",
        _ => "最近 7 天",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realtime_and_scheduled_feeds_have_distinct_fixed_policies() {
        // 即使手动查询配置改成 all，主动推送仍遵循各自独立的数据源策略。
        let cfg = AiNewsConfig {
            limit: 8,
            mode: "all".into(),
            ..Default::default()
        };

        assert_eq!(PushFeed::Realtime.mode(), "all");
        assert_eq!(PushFeed::Curated.mode(), "selected");
        assert_eq!(PushFeed::Realtime.request_limit(&cfg, false, true), 100);
        assert_eq!(PushFeed::Curated.request_limit(&cfg, false, false), 8);
        assert_eq!(PushFeed::Curated.request_limit(&cfg, true, false), 40);
        assert_eq!(PushFeed::Curated.request_limit(&cfg, false, true), 100);
    }

    #[test]
    fn only_pre_send_failures_are_retryable() {
        assert!(retryable_pre_send_error("QQ kernel offline or not ready"));
        assert!(retryable_pre_send_error(
            "QQ session stabilizing; retry after 12s"
        ));
        assert!(retryable_pre_send_error("outbound queue timeout"));
        assert!(!retryable_pre_send_error("request timed out"));
        assert!(!retryable_pre_send_error("connection reset"));
    }
}
