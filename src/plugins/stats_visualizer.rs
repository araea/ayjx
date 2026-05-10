use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::get_prefixes;
use crate::config::build_config;
use crate::db::queries;
use crate::db::utils::get_time_range;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config, word_cloud};
use chrono::Local;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use toml::Value;

mod chart;

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

    #[serde(default)]
    pub daily_push_enabled: bool,
    #[serde(default = "default_daily_push_time")]
    pub daily_push_time: String,
    #[serde(default)]
    pub daily_push_scope: String,
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

fn default_daily_push_time() -> String {
    "23:30:00".to_string()
}

pub fn default_config() -> Value {
    build_config(StatsConfig {
        enabled: true,
        font_path: String::new(),
        font_family: "Noto Sans CJK SC".to_string(),
        width: 960,
        height: 800,
        daily_push_enabled: false,
        daily_push_time: "23:30:00".to_string(),
        daily_push_scope: "本群".to_string(),
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

        if !config.daily_push_enabled {
            return Ok(Some(ctx));
        }

        let scheduler = ctx.scheduler.clone();

        // 调度综合日报推送
        scheduler.schedule_daily_push(
            ctx.clone(),
            writer.clone(),
            "DailyReport",
            config.daily_push_time.clone(),
            move |c, w, gid| async move {
                let date_str = Local::now().format("%Y-%m-%d").to_string();
                let (start, end) = get_time_range("今日");

                // 0. 预检查：判断该群今日是否有消息
                // 如果是冷门群组（无消息），直接跳过
                let count =
                    match queries::get_message_count(&c.db, Some(gid), None, start, end).await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(target: "Plugin/Stats", "查询群 {} 消息记录失败: {}", gid, e);
                            0
                        }
                    };

                if count == 0 {
                    info!(target: "Plugin/Stats", "群 [{}] 今日无消息，跳过推送。", gid);
                    return;
                }

                // 1. 发送提示文本
                info!(target: "Plugin/Stats", "正在推送群 [{}] 日报...", gid);
                let intro_text = format!("📅 [{}] 群数据日报\n📊 正在生成统计数据...", date_str);
                let _ = send_msg(
                    &c,
                    w.clone(),
                    Some(gid),
                    None,
                    Message::new().text(intro_text),
                )
                .await;

                // 2. 生成并发送排行榜 (串行)
                let rank_title = "本群 今日 发言 排行榜".to_string();
                let rank_res = chart::generate(
                    &c,
                    false,
                    "发言",
                    "排行榜",
                    Some(gid),
                    None,
                    0,
                    start,
                    end,
                    &rank_title,
                )
                .await;

                match rank_res {
                    Ok(b64) => {
                        let _ = send_msg(&c, w.clone(), Some(gid), None, Message::new().image(b64))
                            .await;
                    }
                    Err(e) => {
                        warn!(target: "Plugin/Stats", "群 {} 排行榜生成失败: {}", gid, e);
                    }
                }

                // 3. 生成并发送词云 (串行)
                // 调用 word_cloud 模块的公共生成函数
                let wc_res = word_cloud::generate_image(&c, Some(gid), None, start, end).await;

                match wc_res {
                    Ok(b64) => {
                        let _ = send_msg(&c, w.clone(), Some(gid), None, Message::new().image(b64))
                            .await;
                    }
                    Err(e) => {
                        // 词云生成失败（如消息过少）是正常现象，仅记录日志不打扰群
                        info!(target: "Plugin/Stats", "群 {} 词云未生成: {}", gid, e);
                    }
                }

                // 任务结束，scheduler 会自动等待 x 秒后处理下一个群
            },
        );

        Ok(Some(ctx))
    })
}
