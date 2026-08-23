//! 实时推送：资讯一进 AIHOT 精选池就发到群里，而不是等下一个定时档。
//!
//! ## 为什么是轮询
//!
//! AIHOT 官方明确「没有资讯推送通道」——REST / RSS 不提供 Webhook 或流式订阅，
//! MCP 也只在 Agent 主动调用时读数据。所以「实时」只能由我们这端主动问，
//! 差别在于问得聪明不聪明：
//!
//!   - `/api/v1/items` 的 `Cache-Control` 是 `s-maxage=60`，
//!     比 60 秒更密只会拿到同一份 CDN 缓存副本，纯属浪费；
//!   - 带 `If-None-Match` 时，没有新内容的那些轮次服务端只回一个 304 空响应，
//!     这正是官方推荐频繁轮询的原因——绝大多数轮次几乎不产生流量；
//!   - ETag 分区独立（见 [`api::Poll`]），实时轮询不会把定时档的 304 也一并「吃掉」。
//!
//! 官方另有 `/api/v1/selected/snapshot` + `/changes` 增量账本，语义比窗口查询更准，
//! 但它是给「在本地保留一份完整精选副本」的镜像客户端用的：要先分页拉完几千条做基线，
//! 而且按官方说明**不返回 `reason`（推荐理由）字段**——那是卡片上最该先被看见的一句。
//! 群里只需要「刚出的几条」，官方文档也直接建议这类客户端用 `/api/v1/items`，故不采用。
//!
//! ## 怎么保证不吵
//!
//! 实时的代价是节奏不可预期，因此推送前有四道闸：
//!
//!   1. **基线**：每个群第一次被轮询到时只记录时间线（[`state::realtime_status`]），
//!      不推送。否则新装机器或刚开启推送的群会把时间窗里的存量资讯一次性倒出来；
//!   2. **保鲜期**：只推收录时间在 `realtime_max_age_minutes` 内的条目。
//!      Bot 离线一整天再上线时，不会把这一天的旧闻当成「刚刚发生」补发一遍；
//!   3. **单次条数**：一次最多 `realtime_max_items` 条，多出来的留到下一轮；
//!   4. **频次上限**：每个群每小时最多 `realtime_max_per_hour` 次，
//!      再加上 `realtime_quiet_start`—`realtime_quiet_end` 的静默时段（默认深夜不推）。
//!
//! 去重表与定时档共用：实时推过的条目，定时速递不会再推一遍，反之亦然。
//! 静默时段积压下来的资讯不会消失，它们会出现在次日的日报与精选速递里。

use super::api::{self, Item};
use super::pusher;
use super::state;
use super::{AiNewsConfig, LOG_TARGET, load_config, parse_time};
use crate::adapters::onebot::LockedWriter;
use crate::event::Context;
use chrono::{Local, NaiveTime, Utc};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

/// 轮询节拍。真正的抓取间隔由配置决定，这里只是最小检查粒度——
/// 用固定节拍 + 时间戳判断，改了间隔配置无需重启即可生效。
const TICK_SECONDS: u64 = 60;

/// 抓取间隔下限：官方给 `/api/v1/items` 的 `s-maxage` 就是 60 秒
pub(super) const MIN_INTERVAL_SECONDS: u64 = 60;

/// 实时轮询的 ETag 分区，与定时档各记各的
const ETAG_SCOPE: &str = "realtime";

/// 上次真正发起抓取的时刻（Unix 秒）
static LAST_POLL: AtomicI64 = AtomicI64::new(0);
/// 上一轮是否仍在逐群发送中（群多时一轮可能跨好几分钟）
static RUNNING: AtomicBool = AtomicBool::new(false);

/// 确保无论从哪条路径返回，重入标记都会被放开
struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 注册实时轮询任务（连接建立后调用一次）
pub fn spawn(ctx: &Context, writer: &LockedWriter) {
    let cfg = load_config(ctx);
    let interval = cfg.realtime_interval_seconds.max(MIN_INTERVAL_SECONDS);

    if cfg.realtime_enabled {
        info!(
            target: LOG_TARGET,
            "已启用[实时推送]：每 {} 秒条件轮询一次精选池（保鲜 {} 分钟 · 单次至多 {} 条 · 每群每小时至多 {} 次 · 静默 {}—{}）",
            interval,
            cfg.realtime_max_age_minutes.max(1),
            cfg.realtime_max_items.max(1),
            cfg.realtime_max_per_hour.max(1),
            cfg.realtime_quiet_start,
            cfg.realtime_quiet_end
        );
    } else {
        info!(target: LOG_TARGET, "实时推送当前关闭；轮询任务已就位，开启后无需重启即可生效。");
    }
    if cfg.realtime_interval_seconds < MIN_INTERVAL_SECONDS {
        warn!(
            target: LOG_TARGET,
            "realtime_interval_seconds 低于 {} 秒无意义（AIHOT 的缓存就是 {} 秒），已按下限执行。",
            MIN_INTERVAL_SECONDS, MIN_INTERVAL_SECONDS
        );
    }

    let ctx = ctx.clone();
    let writer = writer.clone();
    let scheduler = ctx.scheduler.clone();

    scheduler.add_interval(Duration::from_secs(TICK_SECONDS), move || {
        let ctx = ctx.clone();
        let writer = writer.clone();
        async move { tick(ctx, writer).await }
    });
}

/// 一次节拍：先判断该不该抓，再决定抓不抓
async fn tick(ctx: Context, writer: LockedWriter) {
    let cfg = load_config(&ctx);
    if !cfg.enabled || !cfg.realtime_enabled {
        return;
    }

    let interval = cfg.realtime_interval_seconds.max(MIN_INTERVAL_SECONDS) as i64;
    let now = Utc::now().timestamp();
    if now - LAST_POLL.load(Ordering::Relaxed) < interval {
        return;
    }

    // 静默时段连抓取都省了；积压的资讯会由次日的日报与定时速递带出来
    if in_quiet_hours(
        Local::now().time(),
        &cfg.realtime_quiet_start,
        &cfg.realtime_quiet_end,
    ) {
        return;
    }

    // 服务端要求退避时老实等着，不去凑热闹
    let backoff = api::backoff_seconds_left();
    if backoff > 0 {
        info!(target: LOG_TARGET, "实时推送：AIHOT 要求退避，{} 秒后再试。", backoff);
        return;
    }

    // 上一轮还在逐群发送（群多 + 群间隔）时不重入
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = RunGuard;
    LAST_POLL.store(now, Ordering::Relaxed);

    poll_once(ctx, writer, cfg).await;
}

/// 抓一次精选池，把够新的条目发给还没看过它们的群
async fn poll_once(ctx: Context, writer: LockedWriter, cfg: AiNewsConfig) {
    let targets: Vec<i64> = cfg
        .groups
        .iter()
        .copied()
        .filter(|g| *g != 0 && !cfg.realtime_muted_groups.contains(g))
        .collect();
    if targets.is_empty() {
        return;
    }

    let items = match pusher::fetch_brief(&cfg, api::Poll::Cached(ETAG_SCOPE)).await {
        Ok(Some(items)) => items,
        // 304：精选池没变化。绝大多数轮次都走这里，这正是条件轮询便宜的地方
        Ok(None) => return,
        Err(e) => {
            warn!(target: LOG_TARGET, "实时轮询失败: {}", e);
            return;
        }
    };

    let now = Utc::now().timestamp();
    let max_age = cfg.realtime_max_age_minutes.max(1) * 60;
    // 时间与去重键缺一不可：算不出「有多新」的条目一律不参与实时推送，
    // 宁可漏推一条，也不把来历不明的旧闻当成刚刚发生
    let stamped: Vec<Stamped> = items
        .into_iter()
        .filter_map(|item| {
            Some(Stamped {
                ts: item.discovered_ts()?,
                key: item.dedupe_key()?,
                item,
            })
        })
        .filter(|s| now - s.ts <= max_age)
        .collect();
    if stamped.is_empty() {
        return;
    }

    let max_items = cfg.realtime_max_items.clamp(1, 30);
    let max_per_hour = cfg.realtime_max_per_hour.max(1);
    let mut pushed_any = false;

    for group_id in targets {
        if !pusher::is_allowed(&ctx, group_id) {
            continue;
        }

        let status = state::realtime_status(group_id).await;
        if status.just_primed {
            info!(
                target: LOG_TARGET,
                "群 {} 已建立实时基线，从下一条新资讯开始推送。", group_id
            );
            continue;
        }
        if status.pushes_last_hour >= max_per_hour {
            info!(
                target: LOG_TARGET,
                "群 {} 本小时已实时推送 {} 次，达到上限，剩余条目留给下一小时或定时档。",
                group_id, status.pushes_last_hour
            );
            continue;
        }

        // 基线之后收录的才算「新消息」
        let candidates: Vec<&Stamped> = stamped.iter().filter(|s| s.ts > status.since).collect();
        if candidates.is_empty() {
            continue;
        }

        let keys: Vec<String> = candidates.iter().map(|s| s.key.clone()).collect();
        let unseen = state::unseen_keys(group_id, keys, cfg.dedupe_days).await;
        if unseen.is_empty() {
            continue;
        }

        // 一次最多发 max_items 条；没发出去的仍是「未读」，下一轮或定时档会接着推
        let picked: Vec<&Stamped> = candidates
            .iter()
            .copied()
            .filter(|s| unseen.contains(&s.key))
            .take(max_items)
            .collect();
        let picked_items: Vec<Item> = picked.iter().map(|s| s.item.clone()).collect();
        let picked_keys: Vec<String> = picked.iter().map(|s| s.key.clone()).collect();

        // 群间隔：和定时档一样错开，不让多个群在同一秒收到同一张图
        if pushed_any {
            pusher::pace(&cfg).await;
        }
        pushed_any = true;

        let clock = Local::now().format("%H:%M").to_string();
        let sent = pusher::deliver_items(
            &ctx,
            writer.clone(),
            group_id,
            &cfg,
            pusher::Headline {
                text: &format!("⚡ AI 资讯快报 · {}", clock),
                card_title: "AI 资讯快报",
                card_subtitle: &format!("实时推送 · {}", clock),
            },
            &picked_items,
        )
        .await;

        if sent {
            info!(
                target: LOG_TARGET,
                "实时推送：群 {} 收到 {} 条新资讯。", group_id, picked_items.len()
            );
            state::mark_realtime_sent(group_id, picked_keys).await;
        }
    }
}

/// 一条待推资讯：带上算好的收录时间与去重键，免得在循环里反复解析
struct Stamped {
    ts: i64,
    key: String,
    item: Item,
}

/// 当前是否处于静默时段。
///
/// 起止相同视为「不设静默」；`23:30`—`07:30` 这种跨午夜的区间按跨天处理。
pub(super) fn in_quiet_hours(now: NaiveTime, start: &str, end: &str) -> bool {
    let (Some(start), Some(end)) = (parse_clock(start), parse_clock(end)) else {
        return false;
    };
    if start == end {
        return false;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

/// `HH:MM[:SS]` → [`NaiveTime`]；留空表示不设静默时段
fn parse_clock(raw: &str) -> Option<NaiveTime> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (h, m, s) = parse_time(raw);
    NaiveTime::from_hms_opt(h, m, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).expect("测试时间合法")
    }

    #[test]
    fn quiet_hours_span_midnight() {
        let (start, end) = ("23:30", "07:30");
        assert!(in_quiet_hours(t(23, 45), start, end));
        assert!(in_quiet_hours(t(3, 0), start, end));
        assert!(in_quiet_hours(t(7, 29), start, end));
        assert!(!in_quiet_hours(t(7, 30), start, end));
        assert!(!in_quiet_hours(t(12, 0), start, end));
        assert!(!in_quiet_hours(t(23, 29), start, end));
    }

    #[test]
    fn quiet_hours_within_one_day() {
        let (start, end) = ("09:00", "18:00");
        assert!(in_quiet_hours(t(9, 0), start, end));
        assert!(in_quiet_hours(t(17, 59), start, end));
        assert!(!in_quiet_hours(t(18, 0), start, end));
        assert!(!in_quiet_hours(t(8, 59), start, end));
    }

    #[test]
    fn empty_or_equal_bounds_disable_quiet_hours() {
        assert!(!in_quiet_hours(t(3, 0), "", "07:30"));
        assert!(!in_quiet_hours(t(3, 0), "23:30", ""));
        assert!(!in_quiet_hours(t(3, 0), "00:00", "00:00"));
    }
}
