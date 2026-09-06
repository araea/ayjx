//! ai_news 插件：向指定群聊推送 AI 新资讯（数据源 AIHOT）。
//!
//! 数据源选型：AIHOT 同时提供 RSS 与 v1 REST API，这里选用 **API**——
//! 返回结构化 JSON，无需引入 XML 解析依赖；支持服务端按 `window` / `category` / `q`
//! 筛选；每条带稳定 `id` 便于跨次推送去重；另有 RSS 没有的热点榜与日报端点。
//! 详见 `api.rs` 顶部说明。
//!
//! ## 两条推送线并行
//!
//! **实时快报**（`realtime.rs`）：默认每 60 秒条件轮询一次精选动态池，
//! 精选资讯进池就发，延迟通常在 1 分钟左右——新闻的即时性由这条线负责。
//! AIHOT 没有 Webhook / 流式订阅，「实时」只能靠带 `ETag` 的条件轮询逼近，
//! 没有新内容的轮次服务端只回一个 304，几乎不产生流量。
//! 默认以低干扰为先：只推精选、全分类、按上游允许的 60 秒下限轮询；
//! 确实需要完整信息流时，可用 `/ai实时模式 全部` 临时切回全量池。
//! 基线、去重与持久待发队列负责避免重复和丢失；单批与小时容量保留宽松上限，
//! 防止异常数据造成失控刷屏，详见 `realtime.rs` 顶部说明。
//!
//! **定时档**：日报、精选速递、热点榜按固定排期推送，负责节奏与总结。
//! 两条线分别去重：实时线不漏资讯，定时线仍可把其中的精选内容做成回顾；
//! 同一条内容不会在同一条推送线上反复出现。
//!
//! 默认排期（可在配置或 `/设置` 中调整）。时间点与 `stats` 插件的统计推送
//! 整体错峰，任何一档都不与发言排行榜 / 数据分析撞在同一分钟：
//!
//! | 时间 | 插件 | 内容 |
//! | --- | --- | --- |
//! | 全天 | ai_news | 实时快报（精选池有新条目就推，默认不设静默） |
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
//!   /ai推送添加 <群|私聊> <ID> · /ai推送删除 <群|私聊> <ID>
//!   /ai推送开启 · /ai推送关闭   不带参数时管理当前会话
//!   /ai推送列表 · /ai推送状态 · /ai推送重置
//!   /ai实时开启 · /ai实时关闭   当前或指定目标只收定时档还是也收实时快报
//!   /ai实时模式 <精选|全部>      控制实时快报读取精选池还是全量池
//!   /ai分类 <模型|产品|行业|论文|技巧|全部|默认>   设置当前目标
//!   /ai静默 <HH:MM-HH:MM|关闭|默认>              设置当前目标
//!   /设置 ai_news card_theme auto|light|dark   自动 / 白天 / 夜晚阅读主题
//!
//! 使用边界：AIHOT 的匿名接口可用于个人非商业、公益非商业及组织内部使用；
//! 面向外部的商业产品、数据转售、公开镜像等须先取得 AIHOT 书面授权
//! （https://aihot.virxact.com/terms）。接口返回的标题、摘要等属外部内容，
//! 本插件只作展示，不参与任何指令解析。

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{extract_text_arg, get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config, update_config};
use futures_util::future::BoxFuture;
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use toml::Value;

pub mod api;
mod card;
pub mod leaderboard;
mod pusher;
mod realtime;
mod render;
mod state;

use pusher::Payload;
use render::Rendered;

pub const LOG_TARGET: &str = "Plugin/AiNews";

/// 用户指定的默认推送群
const DEFAULT_GROUP: i64 = 175131947;

// ================= 配置定义 =================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupPreference {
    /// None 继承全局分类；Some("") 表示该目标不限分类
    #[serde(default)]
    pub category: Option<String>,
    /// 两项均为 None 时继承全局静默时段；均为空字符串表示该目标不静默
    #[serde(default)]
    pub quiet_start: Option<String>,
    #[serde(default)]
    pub quiet_end: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiNewsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 推送目标群号列表；可用 `/ai推送添加 群 <群号>` 在任意会话增删
    #[serde(default = "default_groups")]
    pub groups: Vec<i64>,
    /// 推送目标私聊 QQ 号列表；与群聊目标分别存储，避免同号目标混淆
    #[serde(default)]
    pub private_users: Vec<i64>,

    // —— 抓取参数 ——
    /// 手动 `/ai资讯` 查询所用动态池；主动推送的数据源策略不受此项影响
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
    /// 是否附带第三方原文链接（默认开启，同时保留 AIHOT 站内阅读页）
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
    /// 卡片主题：auto（北京时间 07:00—18:59 白天，其余夜晚）/ light / dark
    #[serde(default = "default_card_theme")]
    pub card_theme: String,
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

    // —— 实时推送 ——
    /// 实时快报总开关：动态池一有有效资讯就推，不必等下一个定时档
    #[serde(default = "default_true")]
    pub realtime_enabled: bool,
    /// 实时快报来源：selected（精选，默认低干扰）/ all（全量）
    #[serde(default = "default_realtime_mode")]
    pub realtime_mode: String,
    /// 轮询间隔（秒）。低于 60 秒无意义：AIHOT 的 CDN 缓存就是 60 秒，
    /// 更密只会拿到同一份副本
    #[serde(default = "default_realtime_interval")]
    pub realtime_interval_seconds: u64,
    /// 保鲜期（分钟）：只推收录时间在此之内的条目。
    /// Bot 离线一天再上线时，不会把这一天的旧闻当成「刚刚发生」补发一遍
    #[serde(default = "default_realtime_max_age")]
    pub realtime_max_age_minutes: i64,
    /// 单次实时推送最多几条，多出来的留到下一轮
    #[serde(default = "default_realtime_max_items")]
    pub realtime_max_items: usize,
    /// 每个群每小时最多实时推送几次，防止爆发日刷屏
    #[serde(default = "default_realtime_max_per_hour")]
    pub realtime_max_per_hour: u32,
    /// 静默时段起点（HH:MM）；与终点相同或留空表示不设静默
    #[serde(default = "default_quiet_start")]
    pub realtime_quiet_start: String,
    /// 静默时段终点（HH:MM）。跨午夜按跨天处理
    #[serde(default = "default_quiet_end")]
    pub realtime_quiet_end: String,
    /// 只收定时档、不收实时快报的群；可用 `/ai实时关闭` 在群内增删
    #[serde(default)]
    pub realtime_muted_groups: Vec<i64>,
    /// 只收定时档、不收实时快报的私聊目标
    #[serde(default)]
    pub realtime_muted_private_users: Vec<i64>,
    /// 按目标覆盖分类与静默时段；群聊键沿用群号，私聊键使用 `private:<QQ号>`
    #[serde(default)]
    pub group_preferences: HashMap<String, GroupPreference>,

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
    30
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
fn default_card_theme() -> String {
    "auto".to_string()
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
fn default_realtime_interval() -> u64 {
    60
}
fn default_realtime_mode() -> String {
    "selected".to_string()
}
fn default_realtime_max_age() -> i64 {
    24 * 60
}
fn default_realtime_max_items() -> usize {
    30
}
fn default_realtime_max_per_hour() -> u32 {
    60
}
fn default_quiet_start() -> String {
    String::new()
}
fn default_quiet_end() -> String {
    String::new()
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
            private_users: Vec::new(),
            mode: default_mode(),
            window: default_window(),
            category: String::new(),
            limit: default_limit(),
            min_items: default_min_items(),
            dedupe_days: default_dedupe_days(),
            request_timeout_seconds: default_timeout(),
            summary_max_chars: default_summary_chars(),
            show_reason: true,
            show_original_link: true,
            daily_max_blocks: default_daily_blocks(),
            image_enabled: true,
            image_max_items: default_image_max_items(),
            forward_threshold_chars: default_forward_threshold(),
            forward_node_chars: default_forward_node_chars(),
            image_scale: default_image_scale(),
            card_theme: default_card_theme(),
            send_interval_seconds: default_send_interval(),
            send_interval_max_seconds: default_send_interval_max(),
            leaderboard_max_items: default_leaderboard_items(),
            leaderboard_cache_minutes: default_leaderboard_cache_minutes(),
            realtime_enabled: true,
            realtime_mode: default_realtime_mode(),
            realtime_interval_seconds: default_realtime_interval(),
            realtime_max_age_minutes: default_realtime_max_age(),
            realtime_max_items: default_realtime_max_items(),
            realtime_max_per_hour: default_realtime_max_per_hour(),
            realtime_quiet_start: default_quiet_start(),
            realtime_quiet_end: default_quiet_end(),
            realtime_muted_groups: Vec::new(),
            realtime_muted_private_users: Vec::new(),
            group_preferences: HashMap::new(),
            brief_enabled: true,
            brief_times: default_brief_times(),
            daily_enabled: true,
            daily_time: default_daily_time(),
            hot_topics_enabled: true,
            hot_topics_time: default_hot_topics_time(),
        }
    }
}

impl AiNewsConfig {
    fn target_preference(&self, target: PushTarget) -> Option<&GroupPreference> {
        self.group_preferences.get(&target.preference_key())
    }

    pub(super) fn category_for_target(&self, target: PushTarget) -> &str {
        self.target_preference(target)
            .and_then(|pref| pref.category.as_deref())
            .unwrap_or(&self.category)
            .trim()
    }

    pub(super) fn quiet_for_target(&self, target: PushTarget) -> (&str, &str) {
        let pref = self.target_preference(target);
        let start = pref
            .and_then(|p| p.quiet_start.as_deref())
            .unwrap_or(&self.realtime_quiet_start);
        let end = pref
            .and_then(|p| p.quiet_end.as_deref())
            .unwrap_or(&self.realtime_quiet_end);
        (start.trim(), end.trim())
    }

    fn clean_target_preference(&mut self, target: PushTarget) {
        let key = target.preference_key();
        if self.group_preferences.get(&key).is_some_and(|pref| {
            pref.category.is_none() && pref.quiet_start.is_none() && pref.quiet_end.is_none()
        }) {
            self.group_preferences.remove(&key);
        }
    }

    fn targets(&self) -> Vec<PushTarget> {
        let mut targets = Vec::with_capacity(self.groups.len() + self.private_users.len());
        for id in self.groups.iter().copied().filter(|id| *id > 0) {
            let target = PushTarget::Group(id);
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        for id in self.private_users.iter().copied().filter(|id| *id > 0) {
            let target = PushTarget::Private(id);
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    fn contains_target(&self, target: PushTarget) -> bool {
        match target {
            PushTarget::Group(id) => self.groups.contains(&id),
            PushTarget::Private(id) => self.private_users.contains(&id),
        }
    }

    fn target_realtime_muted(&self, target: PushTarget) -> bool {
        match target {
            PushTarget::Group(id) => self.realtime_muted_groups.contains(&id),
            PushTarget::Private(id) => self.realtime_muted_private_users.contains(&id),
        }
    }

    /// 未知值安全回退到精选，避免手改配置时意外开启全量轰炸。
    pub(super) fn realtime_uses_all(&self) -> bool {
        matches!(
            self.realtime_mode.trim().to_ascii_lowercase().as_str(),
            "all" | "full" | "全部" | "全量"
        )
    }

    fn realtime_mode_label(&self) -> &'static str {
        if self.realtime_uses_all() {
            "全部资讯"
        } else {
            "精选资讯"
        }
    }
}

/// 一个可主动投递的会话。私聊用负数作为内部状态 ID，避免与同号群聊共享去重记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PushTarget {
    Group(i64),
    Private(i64),
}

impl PushTarget {
    fn current(group_id: Option<i64>, user_id: i64) -> Option<Self> {
        group_id
            .filter(|id| *id > 0)
            .map(Self::Group)
            .or_else(|| (user_id > 0).then_some(Self::Private(user_id)))
    }

    pub(super) fn group_id(self) -> Option<i64> {
        match self {
            Self::Group(id) => Some(id),
            Self::Private(_) => None,
        }
    }

    pub(super) fn user_id(self) -> Option<i64> {
        match self {
            Self::Group(_) => None,
            Self::Private(id) => Some(id),
        }
    }

    pub(super) fn state_id(self) -> i64 {
        match self {
            Self::Group(id) => id,
            Self::Private(id) => -id,
        }
    }

    fn preference_key(self) -> String {
        match self {
            Self::Group(id) => id.to_string(),
            Self::Private(id) => format!("private:{}", id),
        }
    }
}

impl fmt::Display for PushTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Group(id) => write!(f, "群聊 {}", id),
            Self::Private(id) => write!(f, "私聊 {}", id),
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
        if config.targets().is_empty() {
            warn!(target: LOG_TARGET, "尚未配置推送会话，定时推送不会发送任何消息。");
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

        // 三类任务始终注册，触发时再读取最新开关。这样 `/设置` 修改 enabled
        // 会即时生效；只有时间点本身的增删改仍需重启后重新排期。
        schedule(
            &ctx,
            &writer,
            &config.daily_time,
            "AI 日报",
            |cfg| cfg.daily_enabled,
            |c, w, cfg, groups| Box::pin(pusher::push_daily(c, w, cfg, groups)),
        );

        for time in &config.brief_times {
            schedule(
                &ctx,
                &writer,
                time,
                "精选速递",
                |cfg| cfg.brief_enabled,
                |c, w, cfg, groups| Box::pin(pusher::push_brief(c, w, cfg, groups)),
            );
        }

        schedule(
            &ctx,
            &writer,
            &config.hot_topics_time,
            "热点榜",
            |cfg| cfg.hot_topics_enabled,
            |c, w, cfg, groups| Box::pin(pusher::push_hot_topics(c, w, cfg, groups)),
        );

        // 实时快报：轮询任务常驻，开关与参数在每次节拍时重新读取，
        // 所以这里不看 realtime_enabled——关掉再打开无需重启
        realtime::spawn(&ctx, &writer);

        Ok(Some(ctx))
    })
}

type PushFn = fn(
    Context,
    LockedWriter,
    AiNewsConfig,
    Vec<PushTarget>,
) -> futures_util::future::BoxFuture<'static, ()>;
type EnabledFn = fn(&AiNewsConfig) -> bool;

/// 注册一个北京时间的每日任务；配置在每次触发时重新读取，改目标无需重启。
///
/// 不依赖宿主机时区：容器或服务器即使运行在 UTC，08:20 仍表示北京时间 08:20。
fn schedule(
    ctx: &Context,
    writer: &LockedWriter,
    time_str: &str,
    label: &str,
    enabled: EnabledFn,
    runner: PushFn,
) {
    let (h, m, s) = parse_time(time_str);
    info!(target: LOG_TARGET, "已计划[{}]推送：每日北京时间 {:02}:{:02}:{:02}", label, h, m, s);

    let ctx = ctx.clone();
    let writer = writer.clone();
    let label = label.to_string();

    ctx.scheduler.clone().add_schedule(
        move |local_now| next_beijing_run(local_now, h, m, s),
        move || {
            let ctx = ctx.clone();
            let writer = writer.clone();
            let label = label.clone();

            async move {
                let config = load_config(&ctx);
                if !config.enabled || !enabled(&config) {
                    return;
                }
                let targets = config.targets();
                if targets.is_empty() {
                    info!(target: LOG_TARGET, "[{}] 没有配置推送会话，跳过。", label);
                    return;
                }

                info!(target: LOG_TARGET, "开始执行[{}]推送，目标会话 {} 个...", label, targets.len());
                runner(ctx, writer, config, targets).await;
                info!(target: LOG_TARGET, "[{}] 推送任务完成。", label);
            }
        },
    );
}

fn next_beijing_run(
    local_now: chrono::DateTime<Local>,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<chrono::DateTime<Local>> {
    let timezone = render::beijing();
    let now = local_now.with_timezone(&timezone);
    let today = now.date_naive();
    let target_today = timezone
        .from_local_datetime(&today.and_hms_opt(hour, minute, second)?)
        .single()?;
    let target = if target_today > now {
        target_today
    } else {
        target_today + chrono::Duration::days(1)
    };
    Some(target.with_timezone(&Local))
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
        let current_target = PushTarget::current(group_id, user_id);

        // 先匹配更长的「推送管理」类指令，避免与查询指令混淆
        for trigger in [
            "ai推送添加",
            "ai推送删除",
            "ai推送开启",
            "ai推送关闭",
            "ai推送列表",
            "ai推送状态",
            "ai推送重置",
            "ai实时开启",
            "ai实时关闭",
            "ai实时模式",
            "ai分类",
            "ai静默",
        ] {
            let Some(matched) = match_command(&ctx, trigger) else {
                continue;
            };
            let arg = extract_text_arg(&matched.args);
            let reply = handle_push_admin(&ctx, trigger, current_target, &arg).await;
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
                _ => query_brief(&config, current_target).await,
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

async fn query_brief(config: &AiNewsConfig, target: Option<PushTarget>) -> Payload {
    let mut scoped_config = config.clone();
    if let Some(target) = target {
        scoped_config.category = config.category_for_target(target).to_string();
    }
    let window = pusher::window_label(&config.window);
    match pusher::fetch_brief(&scoped_config, api::Poll::Fresh).await {
        Ok(Some(items)) if !items.is_empty() => {
            let opts = pusher::render_options(config);
            let rendered =
                render::render_items(&format!("🤖 AI 资讯速递 · {}", window), &items, &opts);
            let html = card::items_card(
                "AI 资讯速递",
                window,
                pusher::card_slice(&items, config),
                &opts,
                card::resolve_theme(&config.card_theme),
            );
            Payload::build(config, rendered, Some(html)).await
        }
        Ok(_) => notice(format!("📭 {}内暂无 AI 资讯。", window)),
        Err(e) => {
            warn!(target: LOG_TARGET, "查询精选失败: {}", e);
            notice(format!("❌ 获取 AI 资讯失败：{}", e))
        }
    }
}

async fn query_hot_topics(config: &AiNewsConfig) -> Payload {
    match api::fetch_hot_topics(config.request_timeout_seconds, api::Poll::Fresh).await {
        Ok(Some(topics)) if !topics.is_empty() => {
            let rendered = render::render_hot_topics(&topics);
            let html = card::hot_topics_card(
                pusher::card_slice(&topics, config),
                card::resolve_theme(&config.card_theme),
            );
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
            let html = card::daily_card(
                &report,
                config.daily_max_blocks,
                card::resolve_theme(&config.card_theme),
            );
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
            let html = card::models_card(
                &board,
                max_items,
                card::resolve_theme(&config.card_theme),
            );
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
                card::resolve_theme(&config.card_theme),
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

async fn handle_push_admin(
    ctx: &Context,
    trigger: &str,
    current_target: Option<PushTarget>,
    arg: &str,
) -> String {
    let config = load_config(ctx);

    if trigger == "ai推送列表" {
        return render_target_list(&config);
    }

    if trigger == "ai推送状态" {
        let target = if arg.trim().is_empty() {
            current_target
        } else {
            match parse_push_target(arg, current_target) {
                Ok(target) => Some(target),
                Err(message) => return message,
            }
        };
        let pending = match target {
            Some(target) => Some(
                state::realtime_pending_count(target.state_id(), config.realtime_max_age_minutes)
                    .await,
            ),
            None => None,
        };
        return render_status(ctx, &config, target, pending);
    }

    if trigger == "ai实时模式" {
        let normalized = match arg.trim().to_ascii_lowercase().as_str() {
            "精选" | "精选资讯" | "selected" | "curated" => "selected",
            "全部" | "全量" | "全部资讯" | "all" | "full" => "all",
            "" | "状态" => {
                return format!(
                    "当前实时快报仅推送{}。用法：/ai实时模式 <精选|全部>。",
                    config.realtime_mode_label()
                );
            }
            _ => return "模式无效。用法：/ai实时模式 <精选|全部>。".to_string(),
        };
        if (normalized == "all") == config.realtime_uses_all() {
            return format!("实时快报当前已经只推送{}。", config.realtime_mode_label());
        }

        let mode = normalized.to_string();
        let targets = config.targets();
        let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
            cfg.realtime_mode = mode;
            cfg
        })
        .await;
        return match result {
            Ok(_) => {
                // 切换数据源时丢弃旧来源留下的待发队列，并从此刻重新建基线，
                // 避免从全量切到精选后仍把先前积压的普通资讯发出去。
                for target in targets {
                    state::align_realtime_baseline(target.state_id()).await;
                }
                if normalized == "all" {
                    "⚠️ 实时快报已切换为全部资讯；消息量会明显增加，可随时用 /ai实时模式 精选 恢复低干扰模式。".to_string()
                } else {
                    "✅ 实时快报已切换为精选资讯，并已清理旧待发队列；日报、精选速递与热点榜不受影响。".to_string()
                }
            }
            Err(e) => format!("❌ 保存配置失败：{}", e),
        };
    }

    let target = if matches!(trigger, "ai分类" | "ai静默") {
        let Some(target) = current_target else {
            return "无法识别当前会话。".to_string();
        };
        target
    } else {
        match parse_push_target(arg, current_target) {
            Ok(target) => target,
            Err(message) => return message,
        }
    };

    match trigger {
        "ai推送添加" | "ai推送开启" => {
            if config.contains_target(target) {
                return format!("{} 已经开启 AI 资讯推送。", target);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                match target {
                    PushTarget::Group(id) if !cfg.groups.contains(&id) => cfg.groups.push(id),
                    PushTarget::Private(id) if !cfg.private_users.contains(&id) => {
                        cfg.private_users.push(id)
                    }
                    _ => {}
                }
                cfg
            })
            .await;
            match result {
                Ok(_) => {
                    state::align_realtime_baseline(target.state_id()).await;
                    format!("✅ 已添加 {} 的 AI 资讯推送权限。", target)
                }
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai推送删除" | "ai推送关闭" => {
            if !config.contains_target(target) {
                return format!("{} 当前未开启 AI 资讯推送。", target);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                match target {
                    PushTarget::Group(id) => {
                        cfg.groups.retain(|item| *item != id);
                        cfg.realtime_muted_groups.retain(|item| *item != id);
                    }
                    PushTarget::Private(id) => {
                        cfg.private_users.retain(|item| *item != id);
                        cfg.realtime_muted_private_users.retain(|item| *item != id);
                    }
                }
                cfg.group_preferences.remove(&target.preference_key());
                cfg
            })
            .await;
            match result {
                Ok(_) => format!("✅ 已删除 {} 的 AI 资讯推送权限。", target),
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai推送重置" => {
            state::reset_group(target.state_id()).await;
            format!(
                "✅ 已清空 {} 的推送去重记录，下次推送会重新发送近期资讯。\
                 实时快报会重新建立基线，只推此刻之后的新资讯。",
                target
            )
        }
        "ai实时开启" => {
            if !config.realtime_enabled {
                let prefix = get_prefixes(ctx)
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "/".into());
                return format!(
                    "⚠️ 实时推送的总开关当前是关闭的。\
                     可用 {}设置 ai_news realtime_enabled true 打开，目标随即生效。",
                    prefix
                );
            }
            if !config.contains_target(target) {
                return format!("{} 尚未开启 AI 资讯推送，请先添加该目标。", target);
            }
            if !config.target_realtime_muted(target) {
                return format!("{} 已经在接收实时快报。", target);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                match target {
                    PushTarget::Group(id) => cfg.realtime_muted_groups.retain(|g| *g != id),
                    PushTarget::Private(id) => {
                        cfg.realtime_muted_private_users.retain(|user| *user != id)
                    }
                }
                cfg
            })
            .await;
            match result {
                Ok(_) => {
                    state::align_realtime_baseline(target.state_id()).await;
                    format!("⚡ 已开启 {} 的实时快报，从现在起的新资讯将及时送达。", target)
                }
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai实时关闭" => {
            if config.target_realtime_muted(target) {
                return format!("{} 当前只接收定时推送。", target);
            }
            let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
                match target {
                    PushTarget::Group(id) if !cfg.realtime_muted_groups.contains(&id) => {
                        cfg.realtime_muted_groups.push(id)
                    }
                    PushTarget::Private(id)
                        if !cfg.realtime_muted_private_users.contains(&id) =>
                    {
                        cfg.realtime_muted_private_users.push(id)
                    }
                    _ => {}
                }
                cfg
            })
            .await;
            match result {
                Ok(_) => format!("✅ 已关闭 {} 的实时快报；日报、精选速递与热点榜照常推送。", target),
                Err(e) => format!("❌ 保存配置失败：{}", e),
            }
        }
        "ai分类" => update_target_category(ctx, target, arg).await,
        "ai静默" => update_target_quiet(ctx, target, arg).await,
        _ => String::new(),
    }
}

fn parse_push_target(raw: &str, current: Option<PushTarget>) -> Result<PushTarget, String> {
    let raw = raw.trim();
    if raw.is_empty() || matches!(raw, "当前" | "本群" | "本会话") {
        return current.ok_or_else(|| "无法识别当前会话，请显式指定目标。".to_string());
    }

    let normalized = raw.replace(['：', ':', ',', '，'], " ");
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    let usage = || {
        "目标格式不正确。用法：/ai推送添加 <群|私聊> <ID>（如：/ai推送添加 群 123456）。"
            .to_string()
    };

    if parts.len() == 1 {
        if let Ok(id) = parse_positive_id(parts[0]) {
            return Ok(PushTarget::Group(id));
        }
        let lowered = parts[0].to_ascii_lowercase();
        for (prefix, private) in [
            ("群聊", false),
            ("群", false),
            ("group", false),
            ("私聊", true),
            ("私信", true),
            ("好友", true),
            ("private", true),
            ("user", true),
        ] {
            if let Some(id) = lowered.strip_prefix(prefix) {
                let id = parse_positive_id(id).map_err(|_| usage())?;
                return Ok(if private {
                    PushTarget::Private(id)
                } else {
                    PushTarget::Group(id)
                });
            }
        }
        return Err(usage());
    }

    if parts.len() != 2 {
        return Err(usage());
    }
    let kind = parts[0].to_ascii_lowercase();
    let id = parse_positive_id(parts[1]).map_err(|_| usage())?;
    match kind.as_str() {
        "群" | "群聊" | "group" | "g" => Ok(PushTarget::Group(id)),
        "私聊" | "私信" | "好友" | "private" | "user" | "u" | "qq" => {
            Ok(PushTarget::Private(id))
        }
        _ => Err(usage()),
    }
}

fn parse_positive_id(raw: &str) -> Result<i64, ()> {
    raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or(())
}

fn render_target_list(config: &AiNewsConfig) -> String {
    let groups: Vec<i64> = config
        .groups
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect();
    let private_users: Vec<i64> = config
        .private_users
        .iter()
        .copied()
        .filter(|id| *id > 0)
        .collect();
    let mut out = format!(
        "🤖 AI 资讯推送目标（{} 个群聊，{} 个私聊）",
        groups.len(),
        private_users.len()
    );
    if groups.is_empty() && private_users.is_empty() {
        out.push_str("\n暂无目标。可发送：/ai推送添加 群 <群号>");
        return out;
    }
    if !groups.is_empty() {
        out.push_str("\n群聊：");
        out.push_str(
            &groups
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、"),
        );
    }
    if !private_users.is_empty() {
        out.push_str("\n私聊：");
        out.push_str(
            &private_users
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("、"),
        );
    }
    out
}

async fn update_target_category(ctx: &Context, target: PushTarget, raw: &str) -> String {
    let config = load_config(ctx);
    if !config.contains_target(target) {
        return format!("{} 尚未开启 AI 资讯推送，请先添加该目标。", target);
    }

    let raw = raw.trim();
    if raw.is_empty() {
        let current = config.category_for_target(target);
        let label = if current.is_empty() {
            "全部分类"
        } else {
            api::category_label(current)
        };
        return format!(
            "{} 当前接收：{}。\n用法：/ai分类 <模型|产品|行业|论文|技巧|全部|默认>",
            target, label
        );
    }

    let category = match raw.to_ascii_lowercase().as_str() {
        "模型" | "ai-models" => Some("ai-models".to_string()),
        "产品" | "ai-products" => Some("ai-products".to_string()),
        "行业" | "industry" => Some("industry".to_string()),
        "论文" | "paper" => Some("paper".to_string()),
        "技巧" | "tip" => Some("tip".to_string()),
        "全部" | "不限" | "all" | "off" => Some(String::new()),
        "默认" | "继承" | "default" | "inherit" => None,
        _ => {
            return "未识别该分类。可选：模型、产品、行业、论文、技巧、全部或默认。"
                .to_string();
        }
    };
    let stored = category.clone();
    let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
        cfg.group_preferences
            .entry(target.preference_key())
            .or_default()
            .category = stored;
        cfg.clean_target_preference(target);
        cfg
    })
    .await;

    match result {
        Ok(_) => {
            // 切换分类时从当前时刻重新起算，避免把新分类中的存量资讯当作实时新闻。
            state::align_realtime_baseline(target.state_id()).await;
            let updated = load_config(ctx);
            let current = updated.category_for_target(target);
            let label = if current.is_empty() {
                "全部分类"
            } else {
                api::category_label(current)
            };
            format!("✅ {} 的资讯分类已设为：{}。", target, label)
        }
        Err(e) => format!("❌ 保存配置失败：{}", e),
    }
}

async fn update_target_quiet(ctx: &Context, target: PushTarget, raw: &str) -> String {
    let config = load_config(ctx);
    if !config.contains_target(target) {
        return format!("{} 尚未开启 AI 资讯推送，请先添加该目标。", target);
    }

    let raw = raw.trim();
    if raw.is_empty() {
        return format!(
            "{} 的实时静默：{}。\n用法：/ai静默 <23:30-07:30|关闭|默认>",
            target,
            quiet_label(&config, Some(target))
        );
    }

    let lowered = raw.to_ascii_lowercase();
    let (start, end, message) = if matches!(lowered.as_str(), "默认" | "继承" | "default" | "inherit") {
        (None, None, "已恢复全局静默时段".to_string())
    } else if matches!(lowered.as_str(), "关闭" | "全天" | "off" | "none") {
        (Some(String::new()), Some(String::new()), "已关闭静默，全天可接收实时快报".to_string())
    } else {
        let normalized = raw.replace(['—', '–', '~', '～'], "-");
        let Some((start, end)) = normalized.split_once('-') else {
            return "时间格式不正确，请使用「23:30-07:30」。".to_string();
        };
        let (Some(start), Some(end)) = (
            realtime::parse_clock(start),
            realtime::parse_clock(end),
        ) else {
            return "时间格式不正确，请使用 00:00—23:59 范围内的时间。".to_string();
        };
        if start == end {
            return "起止时间不能相同；如需全天接收，请发送「/ai静默 关闭」。".to_string();
        }
        let start = start.format("%H:%M").to_string();
        let end = end.format("%H:%M").to_string();
        let message = format!("实时静默时段已设为 {}—{}", start, end);
        (Some(start), Some(end), message)
    };

    let result = update_config::<AiNewsConfig, _>(ctx, "ai_news", move |mut cfg| {
        let preference = cfg
            .group_preferences
            .entry(target.preference_key())
            .or_default();
        preference.quiet_start = start;
        preference.quiet_end = end;
        cfg.clean_target_preference(target);
        cfg
    })
    .await;

    match result {
        Ok(_) => format!("✅ {} {}。", target, message),
        Err(e) => format!("❌ 保存配置失败：{}", e),
    }
}

fn render_status(
    ctx: &Context,
    config: &AiNewsConfig,
    target: Option<PushTarget>,
    pending_items: Option<usize>,
) -> String {
    let prefix = get_prefixes(ctx)
        .first()
        .cloned()
        .unwrap_or_else(|| "/".into());

    let switch = |on: bool| if on { "✅" } else { "⬜" };

    let mut out = String::from("🤖 AI 资讯推送\n");
    out.push_str(&format!(
        "总开关：{} {}\n",
        switch(config.enabled),
        if config.enabled { "已启用" } else { "已禁用" }
    ));

    if let Some(target) = target {
        out.push_str(&format!(
            "目标：{} · {}\n",
            target,
            if config.contains_target(target) {
                "已开启推送"
            } else {
                "未开启推送"
            }
        ));
    }

    out.push_str(&format!(
        "推送目标：{} 个群聊 · {} 个私聊\n",
        config.groups.len(),
        config.private_users.len()
    ));

    out.push_str("\n⚡ 实时快报\n");
    if config.realtime_enabled {
        let target_enabled = target.is_none_or(|target| config.contains_target(target));
        let muted = target.is_some_and(|target| config.target_realtime_muted(target));
        out.push_str(&format!(
            "{} {}\n",
            switch(target_enabled && !muted),
            if !target_enabled {
                "该目标未开启推送"
            } else if muted {
                "该目标已关闭，只收定时档"
            } else {
                "新资讯进池即推"
            }
        ));
        out.push_str(&format!(
            "   来源：{}（/ai实时模式 精选|全部）\n   每{}查一次 · 保鲜 {} 分钟\n   单次至多 {} 条 · 每小时至多 {} 次\n   静默时段：{}\n",
            config.realtime_mode_label(),
            interval_label(
                config
                    .realtime_interval_seconds
                    .max(realtime::MIN_INTERVAL_SECONDS)
            ),
            config.realtime_max_age_minutes.max(1),
            config.realtime_max_items.max(1),
            config.realtime_max_per_hour.max(1),
            quiet_label(config, target)
        ));
        if let Some(pending_items) = pending_items {
            out.push_str(&format!("   待发队列：{} 条\n", pending_items));
        }
    } else {
        out.push_str("⬜ 已关闭（只按下方排期推送）\n");
    }

    out.push_str("\n📅 定时推送\n");
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
    if config.realtime_enabled && config.brief_enabled {
        out.push_str("   （与实时线独立去重，用于精选回顾）\n");
    }
    out.push_str(&format!(
        "{} 热点榜　{}\n",
        switch(config.hot_topics_enabled),
        config.hot_topics_time
    ));
    out.push_str(&format!(
        "多目标间隔 {}—{} 秒随机，不会同一秒齐发\n",
        config.send_interval_seconds, config.send_interval_max_seconds
    ));
    let theme = card::resolve_theme(&config.card_theme);
    let theme_mode = match config.card_theme.trim().to_ascii_lowercase().as_str() {
        "light" | "day" | "白天" | "日间" => "固定白天".to_string(),
        "dark" | "night" | "夜晚" | "夜间" => "固定夜晚".to_string(),
        _ => format!("自动（当前{}）", theme.label()),
    };
    out.push_str(&format!("阅读主题：{}\n", theme_mode));
    if let Some(target) = target {
        let category = config.category_for_target(target);
        out.push_str(&format!(
            "目标分类：{}\n",
            if category.is_empty() {
                "全部"
            } else {
                api::category_label(category)
            }
        ));
    } else if !config.category.trim().is_empty() {
        out.push_str(&format!("全局分类：{}\n", api::category_label(config.category.trim())));
    }
    out.push('\n');
    out.push_str(&format!(
        "⌨️ {p}ai资讯 · {p}ai热点 · {p}ai日报\n   {p}ai模型榜 · {p}ai搜索 <关键词>\n   {p}ai推送添加/删除 <群|私聊> <ID>\n   {p}ai推送列表 · {p}ai实时开启/关闭 · {p}ai实时模式 精选|全部\n   {p}ai分类 · {p}ai静默\n",
        p = prefix
    ));
    out.push_str(api::ATTRIBUTION);
    out
}

/// 轮询间隔的展示文案：整分钟就说分钟，读起来比「每 180 秒」直观
fn interval_label(seconds: u64) -> String {
    if seconds.is_multiple_of(60) {
        format!(" {} 分钟", seconds / 60)
    } else {
        format!(" {} 秒", seconds)
    }
}

/// 静默时段的展示文案：起止相同或留空都表示「不设静默」
fn quiet_label(config: &AiNewsConfig, target: Option<PushTarget>) -> String {
    let (start, end) = target.map_or_else(
        || (
            config.realtime_quiet_start.trim(),
            config.realtime_quiet_end.trim(),
        ),
        |target| config.quiet_for_target(target),
    );
    if start.is_empty() || end.is_empty() || start == end {
        return "不设（全天推送）".to_string();
    }
    format!("{}—{}", start, end)
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
    fn schedule_is_pinned_to_beijing_even_when_host_timezone_differs() {
        use chrono::Utc;

        let before = Utc.with_ymd_and_hms(2026, 9, 5, 0, 19, 0).unwrap();
        let next = next_beijing_run(before.with_timezone(&Local), 8, 20, 0).unwrap();
        assert_eq!(next.with_timezone(&Utc), Utc.with_ymd_and_hms(2026, 9, 5, 0, 20, 0).unwrap());

        let after = Utc.with_ymd_and_hms(2026, 9, 5, 0, 21, 0).unwrap();
        let next = next_beijing_run(after.with_timezone(&Local), 8, 20, 0).unwrap();
        assert_eq!(next.with_timezone(&Utc), Utc.with_ymd_and_hms(2026, 9, 6, 0, 20, 0).unwrap());
    }

    #[test]
    fn default_config_targets_the_requested_group() {
        let config = AiNewsConfig::default();
        assert_eq!(config.groups, vec![DEFAULT_GROUP]);
        assert!(config.enabled);
        assert_eq!(config.mode, "selected");
        assert_eq!(config.window, "24h");
        assert!(config.category.is_empty());
        assert!(config.show_reason && config.show_original_link && config.image_enabled);
        assert!(config.daily_enabled && config.brief_enabled && config.hot_topics_enabled);
        assert!(config.realtime_enabled);
        assert_eq!(config.realtime_mode, "selected");
        assert!(!config.realtime_uses_all());
        assert_eq!(config.realtime_interval_seconds, 60);
        assert_eq!(config.realtime_max_items, 30);
        assert_eq!(config.realtime_max_per_hour, 60);
        assert!(config.realtime_quiet_start.is_empty());
        assert!(config.realtime_quiet_end.is_empty());
        assert!(config.private_users.is_empty());
        assert!(config.realtime_muted_private_users.is_empty());
        assert!(config.group_preferences.is_empty());
    }

    #[test]
    fn default_config_roundtrips_through_toml() {
        let value = default_config();
        let parsed: AiNewsConfig = value.try_into().expect("默认配置应能反序列化");
        assert_eq!(parsed.groups, vec![DEFAULT_GROUP]);
        assert_eq!(parsed.brief_times.len(), 2);
        assert_eq!(parsed.card_theme, "auto");
        // 实时快报默认开启，且轮询间隔不低于 AIHOT 的缓存时长
        assert!(parsed.realtime_enabled);
        assert_eq!(parsed.realtime_mode, "selected");
        assert!(parsed.realtime_interval_seconds >= realtime::MIN_INTERVAL_SECONDS);
        assert!(parsed.realtime_muted_groups.is_empty());
    }

    /// 老配置文件里没有实时相关的键，反序列化必须能落回默认值——
    /// 主程序补全字段前，插件已经会读一次配置
    #[test]
    fn legacy_config_without_realtime_keys_still_parses() {
        let legacy: Value = toml::from_str(
            r#"
            enabled = true
            groups = [123]
            brief_times = ["12:50:00"]
            "#,
        )
        .expect("片段应是合法 TOML");

        let parsed: AiNewsConfig = legacy.try_into().expect("缺字段时应退回默认值");
        assert_eq!(parsed.groups, vec![123]);
        assert!(parsed.realtime_enabled);
        assert_eq!(parsed.realtime_mode, default_realtime_mode());
        assert_eq!(parsed.realtime_max_items, default_realtime_max_items());
        assert_eq!(parsed.realtime_quiet_start, default_quiet_start());
        assert_eq!(parsed.card_theme, default_card_theme());
        assert!(parsed.private_users.is_empty());
        assert!(parsed.realtime_muted_private_users.is_empty());
    }

    #[test]
    fn quiet_label_reads_as_off_when_bounds_are_empty_or_equal() {
        let mut cfg = AiNewsConfig::default();
        assert!(quiet_label(&cfg, None).contains("不设"));

        cfg.realtime_quiet_start = "23:30".into();
        cfg.realtime_quiet_end = "07:30".into();
        assert_eq!(quiet_label(&cfg, None), "23:30—07:30");

        cfg.realtime_quiet_start = String::new();
        assert!(quiet_label(&cfg, None).contains("不设"));

        cfg.realtime_quiet_start = "08:00".into();
        cfg.realtime_quiet_end = "08:00".into();
        assert!(quiet_label(&cfg, None).contains("不设"));
    }

    #[test]
    fn realtime_mode_only_enables_all_for_explicit_values() {
        let mut cfg = AiNewsConfig::default();
        for mode in ["selected", "curated", "精选", "unexpected", ""] {
            cfg.realtime_mode = mode.into();
            assert!(!cfg.realtime_uses_all(), "{mode} 不应开启全量模式");
        }
        for mode in ["all", "FULL", "全部", "全量"] {
            cfg.realtime_mode = mode.into();
            assert!(cfg.realtime_uses_all(), "{mode} 应开启全量模式");
        }
    }

    #[test]
    fn group_preferences_override_and_inherit_global_values() {
        let mut cfg = AiNewsConfig {
            category: "paper".into(),
            ..Default::default()
        };
        cfg.group_preferences.insert(
            "42".into(),
            GroupPreference {
                category: Some("ai-models".into()),
                quiet_start: Some(String::new()),
                quiet_end: Some(String::new()),
            },
        );

        assert_eq!(cfg.category_for_target(PushTarget::Group(42)), "ai-models");
        assert_eq!(cfg.category_for_target(PushTarget::Group(43)), "paper");
        assert!(quiet_label(&cfg, Some(PushTarget::Group(42))).contains("不设"));
        assert!(quiet_label(&cfg, Some(PushTarget::Group(43))).contains("不设"));
    }

    #[test]
    fn parses_current_group_and_explicit_group_or_private_targets() {
        let current_group = Some(PushTarget::Group(42));
        assert_eq!(parse_push_target("", current_group).unwrap(), PushTarget::Group(42));
        assert_eq!(
            parse_push_target("群 123456", current_group).unwrap(),
            PushTarget::Group(123456)
        );
        assert_eq!(
            parse_push_target("group:123456", current_group).unwrap(),
            PushTarget::Group(123456)
        );
        assert_eq!(
            parse_push_target("私聊 654321", current_group).unwrap(),
            PushTarget::Private(654321)
        );
        assert_eq!(
            parse_push_target("private:654321", current_group).unwrap(),
            PushTarget::Private(654321)
        );
        assert!(parse_push_target("私聊 0", current_group).is_err());
        assert!(parse_push_target("不知道 123", current_group).is_err());
    }

    #[test]
    fn group_and_private_targets_have_distinct_state_and_preference_keys() {
        let group = PushTarget::Group(42);
        let private = PushTarget::Private(42);
        assert_eq!(group.state_id(), 42);
        assert_eq!(private.state_id(), -42);
        assert_ne!(group.preference_key(), private.preference_key());
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
            let items = pusher::fetch_brief(&cfg, api::Poll::Fresh)
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
        async fn realtime_endpoint_defaults_to_selected_pool() {
            let cfg = config();
            let items = pusher::fetch_realtime_for_push(&cfg, api::Poll::Fresh)
                .await
                .expect("精选动态接口应可访问")
                .expect("非条件请求必然带响应体");

            assert!(!cfg.realtime_uses_all(), "默认实时模式应为精选");
            assert!(!items.is_empty(), "过去 24 小时的精选动态不应为空");
            assert!(items.len() <= 100, "实时抓取不应超过接口上限");
            assert!(
                items.iter().all(|i| i.dedupe_key().is_some()),
                "每条都应能算出去重键"
            );
        }

        #[tokio::test]
        #[ignore = "需要访问 aihot.virxact.com"]
        async fn hot_topics_endpoint_is_renderable() {
            let topics = api::fetch_hot_topics(config().request_timeout_seconds, api::Poll::Fresh)
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
