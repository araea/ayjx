//! 实时推送：有效资讯一进 AIHOT 全量动态池就发到群里，而不是等下一个定时档。
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
//! 实时的代价是节奏不可预期，因此推送前有五道闸：
//!
//!   1. **基线**：每个群第一次被轮询到时只记录时间线（[`state::realtime_status`]），
//!      不推送。否则新装机器或刚开启推送的群会把时间窗里的存量资讯一次性倒出来；
//!   2. **保鲜期**：只推收录时间在 `realtime_max_age_minutes` 内的条目。
//!      Bot 离线一整天再上线时，不会把这一天的旧闻当成「刚刚发生」补发一遍；
//!   3. **单次条数**：一次最多 `realtime_max_items` 条，多出来的留到下一轮；
//!   4. **容量护栏**：每个群每小时最多 `realtime_max_per_hour` 次；默认值 60
//!      在正常数据量下等同不限制。静默时段默认关闭，可按群主动设置。
//!   5. **持久待发队列**：超过单次条数、撞上频次上限或发送失败的内容先落盘；
//!      后续即使接口一直返回 304 或进程重启，也会继续按节奏投递，过期内容自动淘汰。
//!
//! 实时与定时各自去重：实时线保证每条有效资讯及时送达，定时线仍可把其中的
//! 精选内容整理成回顾；同一条内容不会在同一条推送线上重复出现。
//! 静默时段积压下来的资讯会保留在实时待发队列，恢复后继续投递。
//! 分类和静默时段可按群覆盖，全局配置仍作为没有覆盖时的默认值。

use super::api::{self, Item};
use super::pusher;
use super::state;
use super::{AiNewsConfig, LOG_TARGET, load_config};
use crate::adapters::satori::LockedWriter;
use crate::event::Context;
use chrono::{NaiveTime, Utc};
use std::collections::HashMap;
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
        let quiet = if cfg.realtime_quiet_start.trim().is_empty()
            || cfg.realtime_quiet_end.trim().is_empty()
            || cfg.realtime_quiet_start.trim() == cfg.realtime_quiet_end.trim()
        {
            "不设（全天推送）".to_string()
        } else {
            format!(
                "{}—{}",
                cfg.realtime_quiet_start, cfg.realtime_quiet_end
            )
        };
        info!(
            target: LOG_TARGET,
            "已启用[实时推送]：每 {} 秒条件轮询一次动态池（保鲜 {} 分钟 · 单次容量 {} 条 · 每群每小时容量 {} 次 · 静默 {}）",
            interval,
            cfg.realtime_max_age_minutes.max(1),
            cfg.realtime_max_items.max(1),
            cfg.realtime_max_per_hour.max(1),
            quiet
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

    // 上一轮还在逐群发送（群多 + 群间隔）时不重入
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = RunGuard;
    LAST_POLL.store(now, Ordering::Relaxed);

    poll_once(ctx, writer, cfg).await;
}

/// 抓一次全量动态池，把够新的条目发给还没看过它们的群
async fn poll_once(ctx: Context, writer: LockedWriter, cfg: AiNewsConfig) {
    let clock_now = Utc::now().with_timezone(&super::render::beijing()).time();
    let targets: Vec<i64> = cfg
        .groups
        .iter()
        .copied()
        .filter(|group_id| {
            if *group_id == 0 || cfg.realtime_muted_groups.contains(group_id) {
                return false;
            }
            let (start, end) = cfg.quiet_for_group(*group_id);
            !in_quiet_hours(clock_now, start, end)
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    // 先为每个有效群建立时间线，再请求接口。这样即便接口本轮返回 304，
    // 新加入或刚重新开启实时推送的群也已经完成基线对齐；下一条真正的新资讯
    // 会正常送达，不会在“第一次看到内容变化”时才建基线并被吃掉。
    let mut statuses = HashMap::with_capacity(targets.len());
    for group_id in &targets {
        if !pusher::is_allowed(&ctx, *group_id) {
            continue;
        }
        let status = state::realtime_status(*group_id).await;
        if status.just_primed {
            info!(
                target: LOG_TARGET,
                "群 {} 已建立实时基线，从此刻后的新资讯开始推送。", group_id
            );
        }
        statuses.insert(*group_id, status);
    }
    if statuses.is_empty() {
        return;
    }

    let now = Utc::now().timestamp();
    let max_age = cfg.realtime_max_age_minutes.max(1) * 60;
    // 退避或 304 只跳过抓取，不跳过队列投递；此前积压的资讯仍可继续发送。
    let stamped: Vec<Stamped> = if api::backoff_seconds_left() > 0 {
        info!(target: LOG_TARGET, "实时推送：AIHOT 正在退避，先处理本地待发队列。");
        Vec::new()
    } else {
        match pusher::fetch_realtime_for_push(&cfg, api::Poll::Cached(ETAG_SCOPE)).await {
            Ok(Some(items)) => items
                .into_iter()
                .filter_map(|item| {
                    Some(Stamped {
                        ts: item.discovered_ts()?,
                        key: item.dedupe_key()?,
                        item,
                    })
                })
                .filter(|stamped| now - stamped.ts <= max_age)
                .collect(),
            // 304：没有新快照，但持久队列可能仍有上一轮未发完的内容
            Ok(None) => Vec::new(),
            Err(e) => {
                warn!(target: LOG_TARGET, "实时轮询失败，继续处理本地待发队列: {}", e);
                Vec::new()
            }
        }
    };

    let max_items = cfg.realtime_max_items.clamp(1, 30);
    let max_per_hour = cfg.realtime_max_per_hour.max(1);
    let mut pushed_any = false;

    for group_id in targets {
        let Some(status) = statuses.get(&group_id).copied() else {
            continue;
        };

        let candidates: Vec<(String, Item, i64)> = stamped
            .iter()
            .filter(|stamped| {
                stamped.ts > status.since
                    && pusher::item_matches_group(&cfg, group_id, &stamped.item)
            })
            .map(|stamped| (stamped.key.clone(), stamped.item.clone(), stamped.ts))
            .collect();
        if !candidates.is_empty() {
            let added = state::enqueue_realtime(group_id, candidates, cfg.dedupe_days).await;
            if added > 0 {
                info!(target: LOG_TARGET, "实时推送：群 {} 新增 {} 条待发资讯。", group_id, added);
            }
        }

        if status.pushes_last_hour >= max_per_hour {
            info!(
                target: LOG_TARGET,
                "群 {} 本小时已实时推送 {} 次，达到上限，剩余条目留给下一小时或定时档。",
                group_id, status.pushes_last_hour
            );
            continue;
        }

        // 一次只取一批；发送成功才出队，失败则留到下一轮重试。
        let picked = state::realtime_pending(
            group_id,
            max_items,
            cfg.realtime_max_age_minutes,
        )
        .await;
        if picked.is_empty() {
            continue;
        }
        let picked_keys: Vec<String> = picked.iter().map(|(key, _)| key.clone()).collect();
        let picked_items: Vec<Item> = picked.into_iter().map(|(_, item)| item).collect();

        // 群间隔：和定时档一样错开，不让多个群在同一秒收到同一张图
        if pushed_any {
            pusher::pace(&cfg).await;
        }
        pushed_any = true;

        let clock = Utc::now()
            .with_timezone(&super::render::beijing())
            .format("%H:%M")
            .to_string();
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
pub(super) fn parse_clock(raw: &str) -> Option<NaiveTime> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let h = parts[0].trim().parse().ok()?;
    let m = parts[1].trim().parse().ok()?;
    let s = parts.get(2).map_or(Some(0), |v| v.trim().parse().ok())?;
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
        assert!(!in_quiet_hours(t(3, 0), "not-a-time", "07:30"));
    }
}
