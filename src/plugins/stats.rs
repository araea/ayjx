use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::strip_prefix;
use crate::config::build_config;
use crate::db::utils::get_time_range;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use crate::scheduler::{Pace, PushFrequency};
use chrono::Weekday;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use toml::Value;

mod chart;
mod pusher;

// ================= 配置定义 =================

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct StatsConfig {
    pub enabled: bool,
    /// 字体文件绝对路径。若提供且存在，优先于 `font_family` 使用。
    pub font_path: String,
    pub font_family: String,
    pub width: u32,
    pub height: u32,

    // —— 主动推送总开关与阈值 ——
    /// 群在统计区间内消息数低于此值则跳过推送（避免打扰冷群）
    pub push_min_messages: u64,

    // —— 多群推送节奏 ——
    /// 群与群之间的最小等待秒数
    pub push_group_gap_min_seconds: u64,
    /// 群与群之间的最大等待秒数；实际间隔在 min—max 之间随机取值，
    /// 避免所有群在同一时刻收到推送，也让节奏不那么"机器"
    pub push_group_gap_max_seconds: u64,

    // —— 每日 23:30 当日总结 ——
    pub daily_push_enabled: bool,
    pub daily_push_time: String,

    // —— 每日 09:00 早安回顾（昨日数据） ——
    pub morning_recap_enabled: bool,
    pub morning_recap_time: String,

    // —— 每日 12:30 午间速览（今日上午） ——
    pub noon_brief_enabled: bool,
    pub noon_brief_time: String,

    // —— 每周一 10:00 上周回顾 ——
    pub weekly_recap_enabled: bool,
    pub weekly_recap_time: String,

    // —— 每周日 21:00 周末轻松榜（表情包） ——
    pub weekend_fun_enabled: bool,
    pub weekend_fun_time: String,

    // —— 每月 1 日 10:20 上月回顾（与周一 10:00 的周报错开，1 号恰逢周一时不会挤在一起）——
    pub monthly_recap_enabled: bool,
    pub monthly_recap_time: String,
}






impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            font_path: String::new(),
            font_family: "Noto Sans CJK SC".to_string(),
            width: 960,
            height: 800,
            push_min_messages: 20,
            push_group_gap_min_seconds: 20,
            push_group_gap_max_seconds: 75,
            daily_push_enabled: true,
            daily_push_time: "23:30:00".to_string(),
            morning_recap_enabled: true,
            morning_recap_time: "09:00:00".to_string(),
            noon_brief_enabled: true,
            noon_brief_time: "12:30:00".to_string(),
            weekly_recap_enabled: true,
            weekly_recap_time: "10:00:00".to_string(),
            weekend_fun_enabled: true,
            weekend_fun_time: "21:00:00".to_string(),
            monthly_recap_enabled: true,
            monthly_recap_time: "10:20:00".to_string(),
        }
    }
}

pub fn default_config() -> Value {
    build_config(StatsConfig::default())
}

// ================= 正则匹配 =================

static REGEX_GLOBAL: OnceLock<Regex> = OnceLock::new();
static REGEX_NORMAL: OnceLock<Regex> = OnceLock::new();

fn get_regex_global() -> &'static Regex {
    REGEX_GLOBAL.get_or_init(|| {
        Regex::new(
            r"^所有群(今日|昨日|本周|上周|近7天|近30天|本月|上月|今年|去年|总)发言(排行榜|走势)$",
        )
        .unwrap()
    })
}

fn get_regex_normal() -> &'static Regex {
    REGEX_NORMAL.get_or_init(|| {
        Regex::new(r"^(?:(本群|跨群|我的))?(今日|昨日|本周|上周|近7天|近30天|本月|上月|今年|去年|总)(发言|表情包|消息类型)(排行榜|走势)$")
            .unwrap()
    })
}

// ================= 插件入口 =================

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg = match ctx.as_message() {
            Some(m) => m,
            None => return Ok(Some(ctx)),
        };
        let content = match strip_prefix(&ctx, msg.text()) {
            Some(c) => c,
            None => return Ok(Some(ctx)),
        };

        let (scope, time_str, data_type, chart_type, is_all_groups) =
            if let Some(caps) = get_regex_global().captures(content) {
                let t = caps.get(1).map_or("", |m| m.as_str());
                let c_type = caps.get(2).map_or("", |m| m.as_str());
                ("跨群", t, "发言", c_type, true)
            } else if let Some(caps) = get_regex_normal().captures(content) {
                let s = caps.get(1).map_or("本群", |m| m.as_str());
                let t = caps.get(2).map_or("", |m| m.as_str());
                let d = caps.get(3).map_or("", |m| m.as_str());
                let c = caps.get(4).map_or("", |m| m.as_str());
                let final_scope = if s.is_empty() { "本群" } else { s };
                (final_scope, t, d, c, false)
            } else {
                return Ok(Some(ctx));
            };

        let group_id = msg.group_id();
        let user_id = msg.user_id();

        if scope == "本群" && group_id.is_none() {
            send_msg(
                &ctx,
                writer,
                None,
                Some(user_id),
                r#"请在群聊中使用"本群"相关指令。"#,
            )
            .await?;
            return Ok(None);
        }

        info!(
            target: "Plugin/Stats",
            "Req: Scope={}, Time={}, Data={}, Chart={}, Global={}",
            scope, time_str, data_type, chart_type, is_all_groups
        );

        let (start_time, end_time) = get_time_range(time_str);

        let (query_group, query_user) = match scope {
            "本群" => (group_id, None),
            "跨群" => (None, None),
            "我的" => (None, Some(user_id)),
            _ => (None, None),
        };

        let title = if is_all_groups {
            format!("所有群 {} {} {}", time_str, data_type, chart_type)
        } else {
            format!("{} {} {} {}", scope, time_str, data_type, chart_type)
        };

        let result_img = chart::generate(
            &ctx,
            is_all_groups,
            data_type,
            chart_type,
            query_group,
            query_user,
            user_id,
            start_time,
            end_time,
            &title,
        )
        .await;

        match result_img {
            Ok(b64) => {
                let reply = Message::new().image(b64);
                send_msg(&ctx, writer, group_id, Some(user_id), reply).await?;
            }
            Err(e) => {
                send_msg(
                    &ctx,
                    writer,
                    group_id,
                    Some(user_id),
                    format!("❌ 生成失败：{}", e),
                )
                .await?;
            }
        }

        Ok(None)
    })
}

pub fn on_connected(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let config: StatsConfig = get_config(&ctx, "stats").unwrap_or_default();

        let scheduler = ctx.scheduler.clone();
        let min = config.push_min_messages;
        let pace = Pace::new(
            config.push_group_gap_min_seconds,
            config.push_group_gap_max_seconds,
        );

        // 注册一系列分时段的主动推送任务
        // 每项可独立开关；设计原则：错峰、不打扰冷群、单条推送内按"引言→数字→主榜→走势→副榜→词云"展开
        //
        // 排期与 ai_news 的资讯推送整体错开（见 `plugins::ai_news` 模块文档的时间表），
        // 同一时刻不会有两个插件同时往群里刷图。
        let registrations: [(bool, &str, String, PushFrequency, PushFn); 6] = [
            (
                config.morning_recap_enabled,
                "MorningRecap",
                config.morning_recap_time.clone(),
                PushFrequency::Daily,
                |c, w, gid, m| Box::pin(pusher::push_morning_recap(c, w, gid, m)),
            ),
            (
                config.noon_brief_enabled,
                "NoonBrief",
                config.noon_brief_time.clone(),
                PushFrequency::Daily,
                |c, w, gid, m| Box::pin(pusher::push_noon_brief(c, w, gid, m)),
            ),
            (
                config.daily_push_enabled,
                "DailySummary",
                config.daily_push_time.clone(),
                PushFrequency::Daily,
                |c, w, gid, m| Box::pin(pusher::push_daily_summary(c, w, gid, m)),
            ),
            (
                config.weekly_recap_enabled,
                "WeeklyRecap",
                config.weekly_recap_time.clone(),
                PushFrequency::Weekly(Weekday::Mon),
                |c, w, gid, m| Box::pin(pusher::push_weekly_recap(c, w, gid, m)),
            ),
            (
                config.weekend_fun_enabled,
                "WeekendFun",
                config.weekend_fun_time.clone(),
                PushFrequency::Weekly(Weekday::Sun),
                |c, w, gid, m| Box::pin(pusher::push_weekend_fun(c, w, gid, m)),
            ),
            (
                config.monthly_recap_enabled,
                "MonthlyRecap",
                config.monthly_recap_time.clone(),
                PushFrequency::Monthly(1),
                |c, w, gid, m| Box::pin(pusher::push_monthly_recap(c, w, gid, m)),
            ),
        ];

        for (enabled, label, time_str, freq, runner) in registrations {
            if !enabled {
                continue;
            }
            scheduler.schedule_periodic_push(
                ctx.clone(),
                writer.clone(),
                "Stats",
                label,
                time_str,
                freq,
                pace,
                move |c, w, gid| runner(c, w, gid, min),
            );
        }

        Ok(Some(ctx))
    })
}

type PushFn = fn(
    Context,
    LockedWriter,
    i64,
    u64,
) -> futures_util::future::BoxFuture<'static, ()>;
