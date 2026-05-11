use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::get_prefixes;
use crate::config::build_config;
use crate::db::utils::get_time_range;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use crate::scheduler::PushFrequency;
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
pub struct StatsConfig {
    pub enabled: bool,
    /// 字体文件绝对路径。若提供且存在，优先于 `font_family` 使用。
    #[serde(default)]
    pub font_path: String,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,

    // —— 主动推送总开关与阈值 ——
    /// 群在统计区间内消息数低于此值则跳过推送（避免打扰冷群）
    #[serde(default = "default_push_min_messages")]
    pub push_min_messages: u64,

    // —— 每日 23:30 当日总结 ——
    #[serde(default = "default_true")]
    pub daily_push_enabled: bool,
    #[serde(default = "default_daily_push_time")]
    pub daily_push_time: String,
    /// 兼容字段（暂未使用）
    #[serde(default)]
    pub daily_push_scope: String,

    // —— 每日 09:00 早安回顾（昨日数据） ——
    #[serde(default = "default_true")]
    pub morning_recap_enabled: bool,
    #[serde(default = "default_morning_recap_time")]
    pub morning_recap_time: String,

    // —— 每日 12:30 午间速览（今日上午） ——
    #[serde(default = "default_true")]
    pub noon_brief_enabled: bool,
    #[serde(default = "default_noon_brief_time")]
    pub noon_brief_time: String,

    // —— 每周一 10:00 上周回顾 ——
    #[serde(default = "default_true")]
    pub weekly_recap_enabled: bool,
    #[serde(default = "default_weekly_recap_time")]
    pub weekly_recap_time: String,

    // —— 每周日 21:00 周末轻松榜（表情/类型） ——
    #[serde(default = "default_true")]
    pub weekend_fun_enabled: bool,
    #[serde(default = "default_weekend_fun_time")]
    pub weekend_fun_time: String,

    // —— 每月 1 日 10:00 上月回顾 ——
    #[serde(default = "default_true")]
    pub monthly_recap_enabled: bool,
    #[serde(default = "default_monthly_recap_time")]
    pub monthly_recap_time: String,
}

fn default_true() -> bool {
    true
}

fn default_font_family() -> String {
    "Noto Sans CJK SC".to_string()
}

fn default_width() -> u32 {
    960
}

fn default_height() -> u32 {
    800
}

fn default_push_min_messages() -> u64 {
    20
}

fn default_daily_push_time() -> String {
    "23:30:00".to_string()
}

fn default_morning_recap_time() -> String {
    "09:00:00".to_string()
}

fn default_noon_brief_time() -> String {
    "12:30:00".to_string()
}

fn default_weekly_recap_time() -> String {
    "10:00:00".to_string()
}

fn default_weekend_fun_time() -> String {
    "21:00:00".to_string()
}

fn default_monthly_recap_time() -> String {
    "10:00:00".to_string()
}

pub fn default_config() -> Value {
    build_config(StatsConfig {
        enabled: true,
        font_path: String::new(),
        font_family: "Noto Sans CJK SC".to_string(),
        width: 960,
        height: 800,
        push_min_messages: 20,
        daily_push_enabled: true,
        daily_push_time: "23:30:00".to_string(),
        daily_push_scope: "本群".to_string(),
        morning_recap_enabled: true,
        morning_recap_time: "09:00:00".to_string(),
        noon_brief_enabled: true,
        noon_brief_time: "12:30:00".to_string(),
        weekly_recap_enabled: true,
        weekly_recap_time: "10:00:00".to_string(),
        weekend_fun_enabled: true,
        weekend_fun_time: "21:00:00".to_string(),
        monthly_recap_enabled: true,
        monthly_recap_time: "10:00:00".to_string(),
    })
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
        let text = msg.text();
        let trimmed_text = text.trim();

        let prefixes = get_prefixes(&ctx);
        let mut matched_content = None;

        if prefixes.is_empty() {
            matched_content = Some(trimmed_text);
        } else {
            for prefix in &prefixes {
                if trimmed_text.starts_with(prefix) {
                    matched_content = Some(trimmed_text[prefix.len()..].trim_start());
                    break;
                }
            }
        }

        let content = match matched_content {
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
            let _ = send_msg(
                &ctx,
                writer,
                None,
                Some(user_id),
                r#"请在群聊中使用"本群"相关指令。"#,
            )
            .await;
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
                let _ = send_msg(&ctx, writer, group_id, Some(user_id), reply).await;
            }
            Err(e) => {
                let _ = send_msg(
                    &ctx,
                    writer,
                    group_id,
                    Some(user_id),
                    format!("生成失败: {}", e),
                )
                .await;
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
        let config: StatsConfig = get_config(&ctx, "stats_visualizer")
            .unwrap_or_else(|| serde::Deserialize::deserialize(default_config()).unwrap());

        let scheduler = ctx.scheduler.clone();
        let min = config.push_min_messages;

        // 注册一系列分时段的主动推送任务
        // 每项可独立开关；设计原则：错峰、不打扰冷群、单条推送内按"引言→数字→主榜→走势→副榜→词云"展开
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
