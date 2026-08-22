//! ai_news 插件：定时向指定群聊推送 AI 新资讯（数据源 AIHOT）。
//!
//! 数据源选型：AIHOT 同时提供 RSS 与 v1 REST API，这里选用 **API**——
//! 返回结构化 JSON，无需引入 XML 解析依赖；支持服务端按 `window` / `category` / `q`
//! 筛选；每条带稳定 `id` 便于跨次推送去重；另有 RSS 没有的热点榜与日报端点。
//! 详见 `api.rs` 顶部说明。
//!
//! 默认排期（可在配置或 `/设置` 中调整）。时间点与 `stats` 插件的统计推送
//! 整体错峰，任何一档都不与发言排行榜 / 数据分析撞在同一分钟：
//!
//! | 时间 | 插件 | 内容 |
//! | --- | --- | --- |
//! | 08:20 | ai_news | AI 日报（当期日报，同一期只推一次） |
//! | 09:00 | stats | 早安回顾（昨日） |
//! | 10:00 周一 | stats | 上周回顾 |
//! | 10:20 每月 1 日 | stats | 上月回顾 |
//! | 12:30 | stats | 午间速览 |
//! | 12:50 | ai_news | 精选速递（过去 24 小时精选，按群去重，只推新条目） |
//! | 20:10 | ai_news | 精选速递 |
//! | 21:00 周日 | stats | 周末轻松榜 |
//! | 21:40 | ai_news | 当前热点榜（Top 10 快照） |
//! | 23:30 | stats | 当日总结 |
//!
//! 同一档推送要发给多个群时，群与群之间等待一段随机间隔
//! （`send_interval_seconds` — `send_interval_max_seconds`），
//! 不会所有群在同一秒收到同一张图。
//!
//! 呈现方式：定时推送与指令回复共用一套投递逻辑，先图后文——
//! 先发一张排版好的卡片图（见 `card.rs`），负责好看、好读、好转发；
//! 图片确认发出后再补一条文本，带上每条的 AIHOT 阅读链接，能点能搜能复制。
//!
//! 文本部分超过 `forward_threshold_chars`（默认 500 字）、或前面已经发过图，
//! 就折叠成合并转发，群里只留一个卡片，不刷屏。
//! 卡片渲染失败（浏览器不可用等）会自动退回纯文本，不影响推送。
//!
//! 指令：
//!   /ai资讯 · /ai新闻   立刻查看最近精选
//!   /ai热点             当前热点榜
//!   /ai日报             最新一期 AI 日报
//!   /ai模型榜           AIHOT 大模型排行榜（共识分 Top N）
//!   /ai搜索 <关键词>     按关键词检索
//!   /ai推送开启 · /ai推送关闭 · /ai推送状态 · /ai推送重置
//!
//! 使用边界：AIHOT 的匿名接口可用于个人非商业、公益非商业及组织内部使用；
//! 面向外部的商业产品、数据转售、公开镜像等须先取得 AIHOT 书面授权
//! （https://aihot.virxact.com/terms）。接口返回的标题、摘要等属外部内容，
//! 本插件只作展示，不参与任何指令解析。

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::{get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config, update_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use std::sync::atomic::{AtomicBool, Ordering};
use toml::Value;

pub mod api;
mod card;
pub mod leaderboard;
mod pusher;
mod render;
mod state;

use pusher::Payload;
use render::Rendered;

pub const LOG_TARGET: &str = "Plugin/AiNews";

/// 用户指定的默认推送群
const DEFAULT_GROUP: i64 = 175131947;

// ================= 配置定义 =================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiNewsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 推送目标群号列表；可用 `/ai推送开启` 在群内增删
    #[serde(default = "default_groups")]
    pub groups: Vec<i64>,

    // —— 抓取参数 ——
    /// `selected`（精选，推荐）或 `all`（全部公开动态）
    #[serde(default = "default_mode")]
    pub mode: String,
    /// 时间窗，AIHOT v1 只支持 `24h` 与 `7d`
    #[serde(default = "default_window")]
    pub window: String,
    /// 分类过滤：ai-models / ai-products / industry / paper / tip；留空表示不限
    #[serde(default)]
    pub category: String,
    /// 单次推送最多条数（1—100）
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// 去重后新条目少于该值则本轮不推送，避免只有一条也刷屏
    #[serde(default = "default_min_items")]
    pub min_items: u32,
    /// 同一条资讯的去重记忆天数
    #[serde(default = "default_dedupe_days")]
    pub dedupe_days: i64,
    /// 单次请求超时（秒）
    #[serde(default = "default_timeout")]
    pub request_timeout_seconds: u64,

    // —— 展示参数 ——
    /// 摘要截断长度（按字符计）
    #[serde(default = "default_summary_chars")]
    pub summary_max_chars: usize,
    /// 是否展示 AIHOT 的「推荐理由」
    #[serde(default = "default_true")]
    pub show_reason: bool,
    /// 是否附带第三方原文链接（默认只给 AIHOT 站内阅读页）
    #[serde(default)]
    pub show_original_link: bool,
    /// 日报最多展示的条目数
    #[serde(default = "default_daily_blocks")]
    pub daily_max_blocks: usize,
    /// 是否把资讯排版成卡片图先发一张（随后仍会补一条带链接的文本）
    #[serde(default = "default_true")]
    pub image_enabled: bool,
    /// 卡片图最多画几条，其余交给文本；图太长反而不好读
    #[serde(default = "default_image_max_items")]
    pub image_max_items: usize,
    /// 整条消息超过多少字符就改用合并转发（折叠成一个卡片，避免刷屏）；0 表示永远发纯文本
    #[serde(default = "default_forward_threshold")]
    pub forward_threshold_chars: usize,
    /// 合并转发时单个节点的字符软上限，单条超长的资讯不会被切断
    #[serde(default = "default_forward_node_chars")]
    pub forward_node_chars: usize,
    /// 卡片图的渲染倍率（1.0—4.0）。倍率越高出图越清晰，字也越"实"；
    /// 2.0 是勉强能看，3.0 在手机上放大也不糊
    #[serde(default = "default_image_scale")]
    pub image_scale: f64,
    /// 多个群之间的最小发送间隔（秒），防风控
    #[serde(default = "default_send_interval")]
    pub send_interval_seconds: u64,
    /// 多个群之间的最大发送间隔（秒）；实际间隔在 min—max 间随机，
    /// 避免所有群在同一秒收到推送
    #[serde(default = "default_send_interval_max")]
    pub send_interval_max_seconds: u64,

    // —— 模型榜 ——
    /// `/ai模型榜` 单次展示的模型条数（1—30）
    #[serde(default = "default_leaderboard_items")]
    pub leaderboard_max_items: usize,
    /// 模型榜数据的本地缓存时长（分钟）；榜单每天只更新几次，不必每次指令都抓一遍
    #[serde(default = "default_leaderboard_cache_minutes")]
    pub leaderboard_cache_minutes: u64,

    // —— 排期 ——
    #[serde(default = "default_true")]
    pub brief_enabled: bool,
    /// 精选速递时间点，可配置多个（HH:MM:SS）
    #[serde(default = "default_brief_times")]
    pub brief_times: Vec<String>,

    #[serde(default = "default_true")]
    pub daily_enabled: bool,
    #[serde(default = "default_daily_time")]
    pub daily_time: String,

    #[serde(default = "default_true")]
    pub hot_topics_enabled: bool,
    #[serde(default = "default_hot_topics_time")]
    pub hot_topics_time: String,
}

fn default_true() -> bool {
    true
}
fn default_groups() -> Vec<i64> {
    vec![DEFAULT_GROUP]
}
fn default_mode() -> String {
    "selected".to_string()
}
fn default_window() -> String {
    "24h".to_string()
}
fn default_limit() -> u32 {
    8
}
fn default_min_items() -> u32 {
    1
}
fn default_dedupe_days() -> i64 {
    7
}
fn default_timeout() -> u64 {
    15
}
fn default_summary_chars() -> usize {
    90
}
fn default_daily_blocks() -> usize {
    12
}
fn default_image_max_items() -> usize {
    10
}
fn default_forward_threshold() -> usize {
    500
}
fn default_forward_node_chars() -> usize {
    300
}
fn default_image_scale() -> f64 {
    3.0
}
fn default_send_interval() -> u64 {
    8
}
fn default_send_interval_max() -> u64 {
    35
}
fn default_leaderboard_items() -> usize {
    12
}
fn default_leaderboard_cache_minutes() -> u64 {
    30
}
// 以下时间点与 stats 插件的统计推送错峰，详见模块文档的时间表
fn default_brief_times() -> Vec<String> {
    vec!["12:50:00".to_string(), "20:10:00".to_string()]
}
fn default_daily_time() -> String {
    "08:20:00".to_string()
}
fn default_hot_topics_time() -> String {
    "21:40:00".to_string()
}

impl Default for AiNewsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            groups: default_groups(),
            mode: default_mode(),
            window: default_window(),
            category: String::new(),
            limit: default_limit(),
            min_items: default_min_items(),
            dedupe_days: default_dedupe_days(),
            request_timeout_seconds: default_timeout(),
            summary_max_chars: default_summary_chars(),
            show_reason: true,
            show_original_link: false,
            daily_max_blocks: default_daily_blocks(),
            image_enabled: true,
            image_max_items: default_image_max_items(),
            forward_threshold_chars: default_forward_threshold(),
            forward_node_chars: default_forward_node_chars(),
            image_scale: default_image_scale(),
            send_interval_seconds: default_send_interval(),
            send_interval_max_seconds: default_send_interval_max(),
            leaderboard_max_items: default_leaderboard_items(),
            leaderboard_cache_minutes: default_leaderboard_cache_minutes(),
            brief_enabled: true,
            brief_times: default_brief_times(),
            daily_enabled: true,
            daily_time: default_daily_time(),
            hot_topics_enabled: true,
            hot_topics_time: default_hot_topics_time(),
        }
    }
}

pub fn default_config() -> Value {
    build_config(AiNewsConfig::default())
}

fn load_config(ctx: &Context) -> AiNewsConfig {
    get_config::<AiNewsConfig>(ctx, "ai_news").unwrap_or_default()
}

// ================= 生命周期 =================

/// 防止 Bot 重连时重复注册定时任务
static SCHEDULED: AtomicBool = AtomicBool::new(false);

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        state::preload().await;

        let config = load_config(&ctx);
        if config.groups.is_empty() {
            warn!(target: LOG_TARGET, "尚未配置推送群，定时推送不会发送任何消息。");
        }
        Ok(())
    })
}

pub fn on_connected(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if SCHEDULED.swap(true, Ordering::SeqCst) {
            // 重连时无需再注册一遍
            return Ok(Some(ctx));
        }

        let config = load_config(&ctx);
        warn_on_schedule_conflicts(&ctx, &config);

        if config.daily_enabled {
            schedule(
                &ctx,
                &writer,
                &config.daily_time,
                "AI 日报",
                |c, w, cfg, groups| Box::pin(pusher::push_daily(c, w, cfg, groups)),
            );
        }

        if config.brief_enabled {
            for time in &config.brief_times {
                schedule(&ctx, &writer, time, "精选速递", |c, w, cfg, groups| {
                    Box::pin(pusher::push_brief(c, w, cfg, groups))
                });
            }
        }

        if config.hot_topics_enabled {
            schedule(
                &ctx,
                &writer,
                &config.hot_topics_time,
                "热点榜",
                |c, w, cfg, groups| Box::pin(pusher::push_hot_topics(c, w, cfg, groups)),
            );
        }

        Ok(Some(ctx))
    })
}

type PushFn = fn(
    Context,
    LockedWriter,
    AiNewsConfig,
    Vec<i64>,
) -> futures_util::future::BoxFuture<'static, ()>;

/// 注册一个每日定时任务；配置在每次触发时重新读取，改群号无需重启
fn schedule(ctx: &Context, writer: &LockedWriter, time_str: &str, label: &str, runner: PushFn) {
    let (h, m, s) = parse_time(time_str);
    info!(target: LOG_TARGET, "已计划[{}]推送：每日 {:02}:{:02}:{:02}", label, h, m, s);

    let ctx = ctx.clone();
    let writer = writer.clone();
    let label = label.to_string();

    ctx.scheduler.clone().add_daily_at(h, m, s, move || {
        let ctx = ctx.clone();
        let writer = writer.clone();
        let label = label.clone();

        async move {
            let config = load_config(&ctx);
            if !config.enabled {
                return;
            }
            let groups: Vec<i64> = config.groups.iter().copied().filter(|g| *g != 0).collect();
            if groups.is_empty() {
                info!(target: LOG_TARGET, "[{}] 没有配置推送群，跳过。", label);
                return;
            }

            info!(target: LOG_TARGET, "开始执行[{}]推送，目标群 {} 个...", label, groups.len());
            runner(ctx, writer, config, groups).await;
            info!(target: LOG_TARGET, "[{}] 推送任务完成。", label);
        }
    });
}

/// 启动时检查本插件的排期是否与 `stats` 的统计推送撞在同一分钟。
///
/// 新装机器用的是错开后的默认值，但老配置文件里的旧时间不会被自动改写
/// （主程序只补新字段、不动既有值）。撞车时给一条明确的提示，
/// 说清是哪两档、改哪个键，而不是让人自己去比对两份配置。
fn warn_on_schedule_conflicts(ctx: &Context, cfg: &AiNewsConfig) {
    let Some(stats) = get_config::<crate::plugins::stats::StatsConfig>(ctx, "stats") else {
        return;
    };
    if !stats.enabled {
        return;
    }

    // 只比到分钟：同分钟内两个插件一起发图，就是用户说的"撞车"
    fn hhmm(raw: &str) -> String {
        let (h, m, _) = parse_time(raw);
        format!("{:02}:{:02}", h, m)
    }

    let occupied: Vec<(String, &str)> = [
        (stats.morning_recap_enabled, &stats.morning_recap_time, "统计 · 早安回顾"),
        (stats.noon_brief_enabled, &stats.noon_brief_time, "统计 · 午间速览"),
        (stats.daily_push_enabled, &stats.daily_push_time, "统计 · 当日总结"),
        (stats.weekly_recap_enabled, &stats.weekly_recap_time, "统计 · 上周回顾"),
        (stats.weekend_fun_enabled, &stats.weekend_fun_time, "统计 · 周末轻松榜"),
        (stats.monthly_recap_enabled, &stats.monthly_recap_time, "统计 · 上月回顾"),
    ]
    .into_iter()
    .filter(|(enabled, _, _)| *enabled)
    .map(|(_, time, label)| (hhmm(time), label))
    .collect();

    let mut mine: Vec<(String, &str, &str)> = Vec::new();
    if cfg.daily_enabled {
        mine.push((hhmm(&cfg.daily_time), "AI 日报", "daily_time"));
    }
    if cfg.brief_enabled {
        for time in &cfg.brief_times {
            mine.push((hhmm(time), "精选速递", "brief_times"));
        }
    }
    if cfg.hot_topics_enabled {
        mine.push((hhmm(&cfg.hot_topics_time), "热点榜", "hot_topics_time"));
    }

    for (time, label, key) in mine {
        if let Some((_, other)) = occupied.iter().find(|(t, _)| *t == time) {
            warn!(
                target: LOG_TARGET,
                "[{}] 的推送时间 {} 与 [{}] 相同，两份图会挤在一起。\
                 可修改配置 ai_news.{}（或用 /设置 ai_news {}）错开几分钟。",
                label, time, other, key, key
            );
        }
    }
}

/// 解析 `HH:MM[:SS]`，非法输入退回 09:00:00
fn parse_time(input: &str) -> (u32, u32, u32) {
    let parts: Vec<&str> = input.trim().split(':').collect();
    if parts.len() < 2 {
        warn!(target: LOG_TARGET, "无法解析推送时间 [{}]，已退回 09:00:00。", input);
        return (9, 0, 0);
    }
    let hour = parts[0].trim().parse::<u32>().unwrap_or(9);
    let minute = parts[1].trim().parse::<u32>().unwrap_or(0);
    let second = parts
        .get(2)
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    if hour > 23 || minute > 59 || second > 59 {
        warn!(target: LOG_TARGET, "推送时间 [{}] 超出范围，已退回 09:00:00。", input);
        return (9, 0, 0);
    }
    (hour, minute, second)
}

// ================= 指令 =================

fn extract_text_arg(args: &[simd_json::OwnedValue]) -> String {
    let mut buf = String::new();
    for seg in args {
        if seg.get_str("type") == Some("text")
            && let Some(text) = seg.get("data").and_then(|d| d.get_str("text"))
        {
            buf.push_str(text);
        }
    }
    buf.trim().to_string()
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let Some(msg) = ctx.as_message() else {
            return Ok(Some(ctx));
        };
        // 快速预判：本插件的指令要么含 "ai"，要么含"模型"（模型榜的免前缀别名），
        // 绝大多数群聊消息可在此直接放行，免去逐条指令重复取前缀、遍历消息段的开销
        let text = msg.text();
        if !text.contains("ai") && !text.contains("模型") {
            return Ok(Some(ctx));
        }

        let group_id = msg.group_id();
        let user_id = msg.user_id();
        let message_id = msg.message_id();

        // 先匹配更长的「推送管理」类指令，避免与查询指令混淆
        for trigger in ["ai推送开启", "ai推送关闭", "ai推送状态", "ai推送重置"] {
            if match_command(&ctx, trigger).is_none() {
                continue;
            }
            let reply = handle_push_admin(&ctx, trigger, group_id).await;
            let body = Message::new().reply(message_id).text(reply);
            send_msg(&ctx, writer, group_id, Some(user_id), body).await?;
            return Ok(None);
        }

        // "模型榜" 系列放在前面：它们与资讯类指令不共享前缀，顺序只影响可读性
        for trigger in [
            "ai模型排行榜",
            "ai模型榜",
            "ai大模型排行榜",
            "模型排行榜",
            "模型榜",
            "ai搜索",
            "ai资讯",
            "ai新闻",
            "ai热点",
            "ai日报",
        ] {
            let Some(matched) = match_command(&ctx, trigger) else {
                continue;
            };
            let arg = extract_text_arg(&matched.args);
            let config = load_config(&ctx);

            let payload = match trigger {
                "ai搜索" => query_search(&config, &arg).await,
                "ai热点" => query_hot_topics(&config).await,
                "ai日报" => query_daily(&config).await,
                "ai模型排行榜" | "ai模型榜" | "ai大模型排行榜" | "模型排行榜" | "模型榜" => {
                    query_models(&config).await
                }
                _ => query_brief(&config).await,
            };

            // 先发卡片图，再补一条带链接的合并转发文本
            pusher::deliver(
                &ctx,
                writer,
                group_id,
                Some(user_id),
                &config,
                &payload,
                Some(message_id),
            )
            .await;
            return Ok(None);
        }

        Ok(Some(ctx))
    })
}

/// 提示类回复：只有文本，不配图
fn notice(text: impl Into<String>) -> Payload {
    Payload::text_only(Rendered::plain(text))
}

async fn query_brief(config: &AiNewsConfig) -> Payload {
    let window = pusher::window_label(&config.window);
    match pusher::fetch_brief(config, false).await {
        Ok(Some(items)) if !items.is_empty() => {
            let opts = pusher::render_options(config);
            let rendered =
                render::render_items(&format!("🤖 AI 资讯速递 · {}", window), &items, &opts);
            let html = card::items_card(
                "AI 资讯速递",
                window,
                pusher::card_slice(&items, config),
                &opts,
            );
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(_) => notice(format!("📭 {}内暂无 AI 精选资讯。", window)),
        Err(e) => {
            warn!(target: LOG_TARGET, "查询精选失败: {}", e);
            notice(format!("❌ 获取 AI 资讯失败：{}", e))
        }
    }
}

async fn query_hot_topics(config: &AiNewsConfig) -> Payload {
    match api::fetch_hot_topics(config.request_timeout_seconds, false).await {
        Ok(Some(topics)) if !topics.is_empty() => {
            let rendered = render::render_hot_topics(&topics);
            let html = card::hot_topics_card(pusher::card_slice(&topics, config));
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(_) => notice("📭 当前没有热点条目。"),
        Err(e) => {
            warn!(target: LOG_TARGET, "查询热点榜失败: {}", e);
            notice(format!("❌ 获取 AI 热点榜失败：{}", e))
        }
    }
}

async fn query_daily(config: &AiNewsConfig) -> Payload {
    match api::fetch_latest_daily(config.request_timeout_seconds).await {
        Ok(Some(report)) => {
            let rendered = render::render_daily(&report, config.daily_max_blocks);
            let html = card::daily_card(&report, config.daily_max_blocks);
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(None) => notice("📭 当前没有可用的 AI 日报。"),
        Err(e) => {
            warn!(target: LOG_TARGET, "查询日报失败: {}", e);
            notice(format!("❌ 获取 AI 日报失败：{}", e))
        }
    }
}

/// 模型榜：AIHOT 汇总多家公开评测榜单后的共识分排名
async fn query_models(config: &AiNewsConfig) -> Payload {
    let max_items = config.leaderboard_max_items.clamp(1, 30);
    match leaderboard::fetch_cached(
        config.request_timeout_seconds,
        config.leaderboard_cache_minutes,
    )
    .await
    {
        Ok(board) if !board.entries.is_empty() => {
            let rendered = render::render_models(&board, max_items);
            let html = card::models_card(&board, max_items);
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(_) => notice("📭 AIHOT 模型榜当前没有可展示的条目。"),
        Err(e) => {
            warn!(target: LOG_TARGET, "查询模型榜失败: {}", e);
            notice(format!("❌ 获取 AI 模型排行榜失败：{}", e))
        }
    }
}

async fn query_search(config: &AiNewsConfig, keyword: &str) -> Payload {
    let keyword = keyword.trim();
    if keyword.chars().count() < 2 {
        return notice("用法：/ai搜索 <关键词>（关键词至少 2 个字）");
    }
    if keyword.chars().count() > 200 {
        return notice("关键词太长了，请控制在 200 字以内。");
    }

    match pusher::search(config, keyword).await {
        Ok((items, from_all_pool)) if !items.is_empty() => {
            let opts = pusher::render_options(config);
            let (header, subtitle) = if from_all_pool {
                (
                    format!("🔎 「{}」近 7 天相关动态（未进入精选）", keyword),
                    format!("「{}」· 近 7 天 · 未进入精选", keyword),
                )
            } else {
                (
                    format!("🔎 「{}」近 7 天精选", keyword),
                    format!("「{}」· 近 7 天精选", keyword),
                )
            };
            let rendered = render::render_items(&header, &items, &opts);
            let html = card::items_card(
                "关键词检索",
                &subtitle,
                pusher::card_slice(&items, config),
                &opts,
            );
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(_) => notice(format!("📭 近 7 天没有找到与「{}」相关的 AI 资讯。", keyword)),
        Err(e) => {
            warn!(target: LOG_TARGET, "搜索 [{}] 失败: {}", keyword, e);
            notice(format!("❌ 搜索失败：{}", e))
        }
    }
}

async fn handle_push_admin(ctx: &Context, trigger: &str, group_id: Option<i64>) -> String {
    let config = load_config(ctx);

    if trigger == "ai推送状态" {
        return render_status(ctx, &config, group_id);
    }

    let Some(gid) = group_id else {
        return "该指令只能在群聊中使用。".to_string();
    };

    match trigger {
        "ai推送开启" => {
            if config.groups.contains(&gid) {
                return format!("本群（{}）已经在推送列表中了。", gid);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                if !cfg.groups.contains(&gid) {
                    cfg.groups.push(gid);
                }
                cfg
            })
            .await;
            match result {
                Ok(_) => format!("✅ 已为本群（{}）开启 AI 资讯推送。", gid),
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai推送关闭" => {
            if !config.groups.contains(&gid) {
                return format!("本群（{}）当前未开启推送。", gid);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                cfg.groups.retain(|g| *g != gid);
                cfg
            })
            .await;
            match result {
                Ok(_) => format!("✅ 已关闭本群（{}）的 AI 资讯推送。", gid),
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai推送重置" => {
            state::reset_group(gid).await;
            "✅ 已清空本群的推送去重记录，下次推送会重新发送近期资讯。".to_string()
        }
        _ => String::new(),
    }
}

fn render_status(ctx: &Context, config: &AiNewsConfig, group_id: Option<i64>) -> String {
    let prefix = get_prefixes(ctx)
        .first()
        .cloned()
        .unwrap_or_else(|| "/".into());

    let switch = |on: bool| if on { "✅" } else { "⬜" };

    let mut out = String::from("🤖 AI 资讯推送状态\n———————————————\n");
    out.push_str(&format!(
        "总开关：{} {}\n",
        switch(config.enabled),
        if config.enabled { "已启用" } else { "已禁用" }
    ));

    if let Some(gid) = group_id {
        out.push_str(&format!(
            "本群（{}）：{}\n",
            gid,
            if config.groups.contains(&gid) {
                "已开启推送"
            } else {
                "未开启推送"
            }
        ));
    }

    out.push_str(&format!("推送群数：{} 个\n", config.groups.len()));
    out.push_str("———————————————\n📅 排期\n");
    out.push_str(&format!(
        "{} AI 日报　{}\n",
        switch(config.daily_enabled),
        config.daily_time
    ));
    out.push_str(&format!(
        "{} 精选速递　{}\n   {} · 每次至多 {} 条\n",
        switch(config.brief_enabled),
        config.brief_times.join(" / "),
        pusher::window_label(&config.window),
        config.limit
    ));
    out.push_str(&format!(
        "{} 热点榜　{}\n",
        switch(config.hot_topics_enabled),
        config.hot_topics_time
    ));
    out.push_str(&format!(
        "多群间隔 {}—{} 秒随机，不会同一秒齐发\n",
        config.send_interval_seconds, config.send_interval_max_seconds
    ));
    if !config.category.trim().is_empty() {
        out.push_str(&format!(
            "分类过滤：{}\n",
            api::category_label(config.category.trim())
        ));
    }
    out.push_str("———————————————\n");
    out.push_str(&format!(
        "⌨️ {p}ai资讯 · {p}ai热点 · {p}ai日报\n   {p}ai模型榜 · {p}ai搜索 <关键词>\n",
        p = prefix
    ));
    out.push_str(api::ATTRIBUTION);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times_and_falls_back_on_garbage() {
        assert_eq!(parse_time("08:30:00"), (8, 30, 0));
        assert_eq!(parse_time("12:30"), (12, 30, 0));
        assert_eq!(parse_time("25:00:00"), (9, 0, 0));
        assert_eq!(parse_time("abc"), (9, 0, 0));
    }

    #[test]
    fn default_config_targets_the_requested_group() {
        let config = AiNewsConfig::default();
        assert_eq!(config.groups, vec![DEFAULT_GROUP]);
        assert!(config.enabled);
        assert_eq!(config.mode, "selected");
        assert_eq!(config.window, "24h");
    }

    #[test]
    fn default_config_roundtrips_through_toml() {
        let value = default_config();
        let parsed: AiNewsConfig = value.try_into().expect("默认配置应能反序列化");
        assert_eq!(parsed.groups, vec![DEFAULT_GROUP]);
        assert_eq!(parsed.brief_times.len(), 2);
    }

    /// 联网冒烟测试：默认 `#[ignore]`，不参与常规 `cargo test`。
    ///
    /// 部署前在能访问公网的机器上跑一遍，确认 AIHOT 接口契约与推送排版都正常：
    ///   cargo test plugins::ai_news::tests::live -- --ignored --nocapture
    mod live {
        use super::*;

        fn config() -> AiNewsConfig {
            AiNewsConfig::default()
        }

        #[tokio::test]
        #[ignore = "需要访问 aihot.virxact.com"]
        async fn brief_endpoint_is_renderable() {
            let cfg = config();
            let items = pusher::fetch_brief(&cfg, false)
                .await
                .expect("精选接口应可访问")
                .expect("非条件请求必然带响应体");

            assert!(!items.is_empty(), "过去 24 小时不应为空");
            assert!(
                items.iter().all(|i| i.dedupe_key().is_some()),
                "每条都应能算出去重键"
            );

            let header = format!("🤖 AI 资讯速递 · {}", pusher::window_label(&cfg.window));
            println!(
                "{}",
                render::render_items(&header, &items, &pusher::render_options(&cfg)).to_text()
            );
        }

        #[tokio::test]
        #[ignore = "需要访问 aihot.virxact.com"]
        async fn hot_topics_endpoint_is_renderable() {
            let topics = api::fetch_hot_topics(config().request_timeout_seconds, false)
                .await
                .expect("热点榜接口应可访问")
                .expect("非条件请求必然带响应体");

            assert!(topics.len() <= 10, "热点榜最多 Top 10");
            println!("{}", render::render_hot_topics(&topics).to_text());
        }

        #[tokio::test]
        #[ignore = "需要访问 aihot.virxact.com"]
        async fn daily_endpoint_is_renderable() {
            let cfg = config();
            let report = api::fetch_latest_daily(cfg.request_timeout_seconds)
                .await
                .expect("日报接口应可访问");

            let Some(report) = report else {
                println!("当前没有可用日报（属正常情况）");
                return;
            };
            assert!(report.date.is_some(), "日报应带日期");
            println!("{}", render::render_daily(&report, cfg.daily_max_blocks).to_text());
        }

        #[tokio::test]
        #[ignore = "需要访问 aihot.virxact.com"]
        async fn search_falls_back_to_all_pool() {
            let (items, from_all_pool) = pusher::search(&config(), "OpenAI")
                .await
                .expect("搜索接口应可访问");
            println!("命中 {} 条（来自全量池：{}）", items.len(), from_all_pool);
        }
    }
}
