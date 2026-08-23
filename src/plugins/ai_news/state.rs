//! 推送去重状态：记录每个群已推送过的条目，避免同一条资讯反复刷屏。
//!
//! 状态落盘到 `data/ai_news/state.json`，进程重启后仍然生效；
//! 超过保留期的记录会在每次写入时清理，文件不会无限增长。

use crate::plugins::get_data_dir;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupState {
    #[serde(default)]
    pub seen: Vec<SeenEntry>,
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
    match serde_json::from_str::<State>(&content) {
        Ok(state) => state,
        Err(e) => {
            warn!(target: LOG_TARGET, "去重状态文件解析失败({})，将重新开始记录。", e);
            State::default()
        }
    }
}

async fn save_to_disk(state: &State) {
    let Ok(dir) = get_data_dir("ai_news").await else {
        return;
    };
    let path = dir.join(STATE_FILE);
    match serde_json::to_string(state) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                warn!(target: LOG_TARGET, "去重状态写入失败: {}", e);
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

/// 挑出该群还没推送过的条目（只读判断，顺带清理过期记录）。
///
/// 标记已推送是 `mark_seen` 的职责——只有真正发出去了才记，
/// 否则条目数未达阈值或发送失败时就再也不会补推了。
pub async fn unseen_keys(group_id: i64, keys: Vec<String>, retain_days: i64) -> Vec<String> {
    let cutoff = Utc::now().timestamp() - retain_days.max(1) * 86_400;

    with_state(move |state| {
        let entry = state.groups.entry(group_id.to_string()).or_default();
        entry.seen.retain(|s| s.ts >= cutoff);

        keys.into_iter()
            .filter(|key| !entry.seen.iter().any(|s| &s.key == key))
            .collect()
    })
    .await
}

/// 记录这些条目已经推送给该群
pub async fn mark_seen(group_id: i64, keys: Vec<String>) {
    let now = Utc::now().timestamp();
    with_state(move |state| {
        remember(state.groups.entry(group_id.to_string()).or_default(), keys, now);
    })
    .await
}

/// 把条目计入去重表（已在表内的不重复记）
fn remember(entry: &mut GroupState, keys: Vec<String>, now: i64) {
    for key in keys {
        if entry.seen.iter().any(|s| s.key == key) {
            continue;
        }
        entry.seen.push(SeenEntry { key, ts: now });
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
        remember(entry, keys, now);
        entry.realtime_pushes.push(now);
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
