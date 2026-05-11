//! 主动推送任务集合：按"引言 → 关键数字 → 主榜 → 走势 → 副榜 → 词云"的顺序串行渲染。
//! 每种推送配套独立的时机（早/午/晚/周/月），由 `on_connected` 注册到调度器。

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::db::queries;
use crate::db::utils::get_time_range;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::stats_visualizer::chart;
use crate::plugins::word_cloud;
use chrono::{Datelike, Duration, Local};

const LOG_TARGET: &str = "Plugin/Stats";

// ================= 通用工具 =================

async fn send_text(c: &Context, w: LockedWriter, gid: i64, text: String) {
    let _ = send_msg(c, w, Some(gid), None, Message::new().text(text)).await;
}

async fn send_chart(
    c: &Context,
    w: LockedWriter,
    gid: i64,
    data_type: &str,
    chart_type: &str,
    range: (i64, i64),
    title: &str,
) {
    match chart::generate(
        c, false, data_type, chart_type,
        Some(gid), None, 0,
        range.0, range.1, title,
    ).await {
        Ok(b64) => {
            let _ = send_msg(c, w, Some(gid), None, Message::new().image(b64)).await;
        }
        Err(e) => {
            warn!(target: LOG_TARGET, "群 {} 「{}」生成失败: {}", gid, title, e);
        }
    }
}

async fn send_wordcloud(c: &Context, w: LockedWriter, gid: i64, range: (i64, i64)) {
    match word_cloud::generate_image(c, Some(gid), None, range.0, range.1).await {
        Ok(b64) => {
            let _ = send_msg(c, w, Some(gid), None, Message::new().image(b64)).await;
        }
        Err(e) => {
            // 词云失败（消息过少等）属正常现象，仅记录日志，不打扰群
            info!(target: LOG_TARGET, "群 {} 词云未生成: {}", gid, e);
        }
    }
}

/// 阈值预检：低于阈值返回 None，调用方据此跳过该群推送
async fn precheck(
    c: &Context,
    gid: i64,
    range: (i64, i64),
    min: u64,
    label: &str,
) -> Option<u64> {
    match queries::get_message_count(&c.db, Some(gid), None, range.0, range.1).await {
        Ok(count) => {
            if count < min {
                info!(target: LOG_TARGET,
                    "群 [{}] {} 区间消息数 {} < 阈值 {}, 跳过。",
                    gid, label, count, min);
                None
            } else {
                Some(count)
            }
        }
        Err(e) => {
            warn!(target: LOG_TARGET, "群 {} 查询消息数失败 ({}): {}", gid, label, e);
            None
        }
    }
}

async fn active_users(c: &Context, gid: i64, range: (i64, i64)) -> u64 {
    queries::get_active_user_count(&c.db, Some(gid), range.0, range.1)
        .await
        .unwrap_or(0)
}

// ================= 各推送任务 =================

/// [23:30 每日] 当日总结：发言榜 + 词云
pub async fn push_daily_summary(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("今日");
    let label = "当日总结";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };
    let users = active_users(&c, gid, range).await;
    let date_str = Local::now().format("%Y-%m-%d").to_string();

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "📅 {} · 今日群聊小结\n📊 全天共 {} 条消息，{} 位群友活跃。",
        date_str, count, users
    )).await;
    send_chart(&c, w.clone(), gid, "发言", "排行榜", range, "本群 今日 发言 排行榜").await;
    send_wordcloud(&c, w, gid, range).await;
}

/// [09:00 每日] 早安回顾：昨日发言榜 + 昨日 24h 走势
pub async fn push_morning_recap(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("昨日");
    let label = "早安回顾";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };
    let users = active_users(&c, gid, range).await;
    let yest = (Local::now() - Duration::days(1)).format("%m月%d日").to_string();

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "🌅 早安！昨日（{}）群活跃回顾\n📊 共 {} 条消息，{} 位群友参与。",
        yest, count, users
    )).await;
    send_chart(&c, w.clone(), gid, "发言", "排行榜", range, "本群 昨日 发言 排行榜").await;
    send_chart(&c, w, gid, "发言", "走势", range, "本群 昨日 24小时活跃走势").await;
}

/// [12:30 每日] 午间速览：今日上半场发言榜
pub async fn push_noon_brief(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("今日");
    let label = "午间速览";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "☀️ 中午好！今日上半场战报\n📊 截至现在共 {} 条发言。",
        count
    )).await;
    send_chart(&c, w, gid, "发言", "排行榜", range, "本群 今日上午 发言 排行榜").await;
}

/// [周一 10:00] 上周回顾：上周发言榜 + 上周走势
pub async fn push_weekly_recap(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("上周");
    let label = "上周回顾";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };
    let users = active_users(&c, gid, range).await;

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "🗓 新一周开工！上周群聊回顾\n📊 全周 {} 条消息，{} 位群友活跃。",
        count, users
    )).await;
    send_chart(&c, w.clone(), gid, "发言", "排行榜", range, "本群 上周 发言 排行榜").await;
    send_chart(&c, w, gid, "发言", "走势", range, "本群 上周 发言 走势").await;
}

/// [周日 21:00] 周末轻松榜：本周表情包 + 本周消息类型分布
pub async fn push_weekend_fun(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("本周");
    let label = "周末轻松榜";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "🎉 周末晚好！本周轻松一刻（共 {} 条消息）\n📊 看看这周谁最爱用表情包，大家都在聊什么类型。",
        count
    )).await;
    send_chart(&c, w.clone(), gid, "表情包", "排行榜", range, "本群 本周 表情包 排行榜").await;
    send_chart(&c, w, gid, "消息类型", "排行榜", range, "本群 本周 消息类型 分布").await;
}

/// [每月 1 日 10:00] 上月回顾：上月发言榜 + 上月走势 + 上月词云
pub async fn push_monthly_recap(c: Context, w: LockedWriter, gid: i64, min: u64) {
    let range = get_time_range("上月");
    let label = "上月回顾";

    let count = match precheck(&c, gid, range, min, label).await {
        Some(n) => n,
        None => return,
    };
    let users = active_users(&c, gid, range).await;
    let now = Local::now();
    let last_month = if now.month() == 1 { 12 } else { now.month() - 1 };

    info!(target: LOG_TARGET, "推送群 [{}] {}...", gid, label);

    send_text(&c, w.clone(), gid, format!(
        "📆 月度回顾 · {}月\n📊 上月共 {} 条消息，{} 位群友活跃。",
        last_month, count, users
    )).await;
    send_chart(&c, w.clone(), gid, "发言", "排行榜", range, "本群 上月 发言 排行榜").await;
    send_chart(&c, w.clone(), gid, "发言", "走势", range, "本群 上月 发言 走势").await;
    send_wordcloud(&c, w, gid, range).await;
}
