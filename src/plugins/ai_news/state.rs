//! 推送状态：分别记录每个群在实时线与定时线已推送的条目，并维护实时待发队列。
//!
//! 状态落盘到 `data/ai_news/state.json`，进程重启后仍然生效；
//! 超过保留期的记录会在每次写入时清理，文件不会无限增长。

use super::api::Item;
use crate::plugins::get_data_dir;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;

const LOG_TARGET: &str = "Plugin/AiNews";
const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeenEntry {
    /// 条目去重键（见 `api::Item::dedupe_key`）
    pub key: String,
    /// 推送时的 Unix 时间戳（秒）
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEntry {
    pub key: String,
    pub item: Item,
    /// AIHOT 收录时间，用于保鲜淘汰和按时间顺序出队
    pub discovered_ts: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupState {
    /// 实时快报去重记录。`seen` 是升级前实时/定时共用字段的兼容别名。
    #[serde(default, alias = "seen")]
    pub realtime_seen: Vec<SeenEntry>,
    /// 定时精选去重记录；与实时线分离，确保精选内容仍能按时形成回顾。
    #[serde(default)]
    pub brief_seen: Vec<SeenEntry>,
    /// 已推送过的最新日报日期，防止同一期日报重复推送
    #[serde(default)]
    pub last_daily_date: Option<String>,
    /// 实时推送的基线时间（Unix 秒）：只有收录时间晚于它的资讯才会被实时推送。
    ///
    /// 第一次轮询到本群时建立，之后不再变动——没有它，新装机器或刚开启推送的群
    /// 会把时间窗内的存量资讯当成「新消息」一次性倒出来。
    #[serde(default)]
    pub realtime_since: Option<i64>,
    /// 最近若干次实时推送的时间戳（Unix 秒），用于每小时频次上限；只保留最近一小时
    #[serde(default)]
    pub realtime_pushes: Vec<i64>,
    /// 已抓到但因单次条数、频次上限或发送失败尚未送达的实时资讯
    #[serde(default)]
    pub realtime_pending: Vec<PendingEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub groups: HashMap<String, GroupState>,
}

static STORE: OnceLock<AsyncMutex<Option<State>>> = OnceLock::new();

fn store() -> &'static AsyncMutex<Option<State>> {
    STORE.get_or_init(|| AsyncMutex::new(None))
}

async fn load_from_disk() -> State {
    let Ok(dir) = get_data_dir("ai_news").await else {
        warn!(target: LOG_TARGET, "无法创建数据目录，去重状态本次仅驻留内存。");
        return State::default();
    };
    let path = dir.join(STATE_FILE);
    let Ok(content) = tokio::fs::read_to_string(&path).await else {
        return State::default();
    };
    match parse_state(&content) {
        Ok(state) => state,
        Err(e) => {
            warn!(target: LOG_TARGET, "去重状态文件解析失败({})，将重新开始记录。", e);
            State::default()
        }
    }
}

/// 旧版只有一份 `seen`。首次升级时把它同时作为两条线的历史基线，既完成
/// 去重域拆分，也不会让进程重启后立刻重发最近几天的内容。
fn parse_state(content: &str) -> Result<State, serde_json::Error> {
    let raw: serde_json::Value = serde_json::from_str(content)?;
    let mut state: State = serde_json::from_value(raw.clone())?;
    for (group_id, group) in &mut state.groups {
        let has_brief_history = raw
            .get("groups")
            .and_then(|groups| groups.get(group_id))
            .and_then(|value| value.get("brief_seen"))
            .is_some();
        if !has_brief_history {
            group.brief_seen = group.realtime_seen.clone();
        }
    }
    Ok(state)
}

async fn save_to_disk(state: &State) {
    let Ok(dir) = get_data_dir("ai_news").await else {
        return;
    };
    let path = dir.join(STATE_FILE);
    let temp_path = dir.join(format!("{}.tmp", STATE_FILE));
    match serde_json::to_string(state) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&temp_path, json).await {
                warn!(target: LOG_TARGET, "去重状态写入失败: {}", e);
            } else if let Err(e) = tokio::fs::rename(&temp_path, &path).await {
                warn!(target: LOG_TARGET, "去重状态原子替换失败: {}", e);
            }
        }
        Err(e) => warn!(target: LOG_TARGET, "去重状态序列化失败: {}", e),
    }
}

/// 在全局锁内读改写状态，并把结果落盘
async fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = store().lock().await;
    if guard.is_none() {
        *guard = Some(load_from_disk().await);
    }
    let state = guard.as_mut().expect("状态已在上一步初始化");
    let result = f(state);
    let snapshot = state.clone();
    save_to_disk(&snapshot).await;
    result
}

/// 预加载状态文件（插件初始化时调用，避免首次推送时才读盘）
pub async fn preload() {
    let mut guard = store().lock().await;
    if guard.is_none() {
        *guard = Some(load_from_disk().await);
    }
}

/// 挑出该群尚未在定时精选中推送过的条目（顺带清理过期记录）。
///
/// 标记已推送是 `mark_brief_seen` 的职责——只有真正发出去了才记，
/// 否则条目数未达阈值或发送失败时就再也不会补推了。
pub async fn unseen_brief_keys(
    group_id: i64,
    keys: Vec<String>,
    retain_days: i64,
) -> Vec<String> {
    let cutoff = Utc::now().timestamp() - retain_days.max(1) * 86_400;

    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        entry.brief_seen.retain(|s| s.ts >= cutoff);

        keys.into_iter()
            .filter(|key| !entry.brief_seen.iter().any(|s| &s.key == key))
            .collect()
    })
    .await
}

/// 记录这些条目已经通过定时精选推送给该群。
pub async fn mark_brief_seen(group_id: i64, keys: Vec<String>) {
    let now = Utc::now().timestamp();
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        remember(&mut entry.brief_seen, keys, now);
    })
    .await
}

/// 把条目计入去重表（已在表内的不重复记）
fn remember(history: &mut Vec<SeenEntry>, keys: Vec<String>, now: i64) {
    for key in keys {
        if history.iter().any(|s| s.key == key) {
            continue;
        }
        history.push(SeenEntry { key, ts: now });
    }
}

/// 某个群的实时推送状态（一次加锁取齐，避免逐项读写反复落盘）
#[derive(Debug, Clone, Copy)]
pub struct RealtimeStatus {
    /// 实时推送基线：只推收录时间晚于它的资讯
    pub since: i64,
    /// 本次调用刚刚建立基线——说明这是该群的第一轮，只对齐时间线，不推送
    pub just_primed: bool,
    /// 最近一小时内已经实时推送过几次
    pub pushes_last_hour: u32,
}

/// 读取该群的实时推送状态；首次调用会以当前时刻建立基线
pub async fn realtime_status(group_id: i64) -> RealtimeStatus {
    let now = Utc::now().timestamp();
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        entry.realtime_pushes.retain(|ts| *ts > now - 3_600);

        let just_primed = entry.realtime_since.is_none();
        let since = *entry.realtime_since.get_or_insert(now);

        RealtimeStatus {
            since,
            just_primed,
            pushes_last_hour: entry.realtime_pushes.len() as u32,
        }
    })
    .await
}

/// 记录一次实时推送：条目计入去重，同时留下一个时间戳供频次上限统计
pub async fn mark_realtime_sent(group_id: i64, keys: Vec<String>) {
    let now = Utc::now().timestamp();
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        let sent: HashSet<&str> = keys.iter().map(String::as_str).collect();
        entry
            .realtime_pending
            .retain(|pending| !sent.contains(pending.key.as_str()));
        remember(&mut entry.realtime_seen, keys, now);
        entry.realtime_pushes.push(now);
    })
    .await
}

/// 把新抓到的实时资讯并入持久队列；已发送或已在队列中的条目不会重复加入。
pub async fn enqueue_realtime(
    group_id: i64,
    items: Vec<(String, Item, i64)>,
    retain_days: i64,
) -> usize {
    let cutoff = Utc::now().timestamp() - retain_days.max(1) * 86_400;
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        enqueue_pending(entry, items, cutoff)
    })
    .await
}

fn enqueue_pending(
    entry: &mut GroupState,
    items: Vec<(String, Item, i64)>,
    seen_cutoff: i64,
) -> usize {
    entry
        .realtime_seen
        .retain(|seen| seen.ts >= seen_cutoff);

    let mut added = 0;
    for (key, item, discovered_ts) in items {
        if entry.realtime_seen.iter().any(|seen| seen.key == key)
            || entry.realtime_pending.iter().any(|pending| pending.key == key)
        {
            continue;
        }
        entry.realtime_pending.push(PendingEntry {
            key,
            item,
            discovered_ts,
        });
        added += 1;
    }
    entry.realtime_pending.sort_by_key(|pending| pending.discovered_ts);
    added
}

/// 查看下一批待发资讯。过期条目以及已被定时档送达的条目会在这里淘汰。
pub async fn realtime_pending(
    group_id: i64,
    max_items: usize,
    max_age_minutes: i64,
) -> Vec<(String, Item)> {
    let cutoff = Utc::now().timestamp() - max_age_minutes.max(1) * 60;
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        next_pending(entry, max_items, cutoff)
    })
    .await
}

pub async fn realtime_pending_count(group_id: i64, max_age_minutes: i64) -> usize {
    let cutoff = Utc::now().timestamp() - max_age_minutes.max(1) * 60;
    with_state(move |state| {
        state
            .groups
            .get_mut(&group_id.to_string())
            .map_or(0, |entry| {
                prune_pending(entry, cutoff);
                entry.realtime_pending.len()
            })
    })
    .await
}

fn next_pending(
    entry: &mut GroupState,
    max_items: usize,
    freshness_cutoff: i64,
) -> Vec<(String, Item)> {
    prune_pending(entry, freshness_cutoff);
    entry
        .realtime_pending
        .iter()
        .take(max_items.max(1))
        .map(|pending| (pending.key.clone(), pending.item.clone()))
        .collect()
}

fn prune_pending(entry: &mut GroupState, freshness_cutoff: i64) {
    let seen: HashSet<&str> = entry
        .realtime_seen
        .iter()
        .map(|seen| seen.key.as_str())
        .collect();
    entry.realtime_pending.retain(|pending| {
        pending.discovered_ts >= freshness_cutoff && !seen.contains(pending.key.as_str())
    });
}

/// 把实时基线对齐到当前时刻，并清空旧频次窗口。
///
/// 群暂停实时快报后重新开启时调用，确保暂停期间积压的条目不会突然集中补发。
pub async fn align_realtime_baseline(group_id: i64) {
    let now = Utc::now().timestamp();
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        entry.realtime_since = Some(now);
        entry.realtime_pushes.clear();
        entry.realtime_pending.clear();
    })
    .await
}

/// 该群是否已经推送过这一期日报
pub async fn has_pushed_daily(group_id: i64, date: &str) -> bool {
    let date = date.to_string();
    with_state(move |state| {
        state
            .groups
            .get(&group_id.to_string())
            .and_then(|g| g.last_daily_date.as_deref())
            == Some(date.as_str())
    })
    .await
}

/// 记录该群已推送的日报期号
pub async fn mark_daily(group_id: i64, date: &str) {
    let date = date.to_string();
    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        entry.last_daily_date = Some(date);
    })
    .await
}

/// 清空某个群的去重记录（用于 `/ai推送重置`，便于重新推送一遍）
pub async fn reset_group(group_id: i64) {
    with_state(move |state| {
        state.groups.remove(&group_id.to_string());
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Item {
        Item {
            id: Some(id.to_string()),
            title: Some(format!("资讯 {}", id)),
            ..Default::default()
        }
    }

    #[test]
    fn pending_queue_is_deduplicated_ordered_and_fresh() {
        let mut group = GroupState::default();
        group.realtime_seen.push(SeenEntry {
            key: "id:seen".into(),
            ts: 100,
        });

        let added = enqueue_pending(
            &mut group,
            vec![
                ("id:newer".into(), item("newer"), 300),
                ("id:older".into(), item("older"), 200),
                ("id:seen".into(), item("seen"), 250),
                ("id:newer".into(), item("newer"), 300),
            ],
            0,
        );
        assert_eq!(added, 2);
        assert_eq!(next_pending(&mut group, 1, 0)[0].0, "id:older");

        group.realtime_seen.push(SeenEntry {
            key: "id:older".into(),
            ts: 400,
        });
        let remaining = next_pending(&mut group, 5, 250);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, "id:newer");
    }

    #[test]
    fn legacy_state_without_pending_queue_still_parses() {
        let state = parse_state(
            r#"{"groups":{"42":{"seen":[{"key":"id:old","ts":100}],"realtime_since":123,"realtime_pushes":[]}}}"#,
        )
        .unwrap();
        assert!(state.groups["42"].realtime_pending.is_empty());
        assert_eq!(state.groups["42"].realtime_seen.len(), 1);
        assert_eq!(state.groups["42"].brief_seen.len(), 1);
    }

    #[test]
    fn realtime_and_brief_histories_are_independent() {
        let mut group = GroupState::default();
        remember(&mut group.realtime_seen, vec!["id:same".into()], 100);
        assert!(group.brief_seen.is_empty());

        remember(&mut group.brief_seen, vec!["id:same".into()], 200);
        assert_eq!(group.realtime_seen[0].ts, 100);
        assert_eq!(group.brief_seen[0].ts, 200);
    }
}
