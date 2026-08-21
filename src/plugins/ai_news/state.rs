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
        let entry = state.groups.entry(group_id.to_string()).or_default();
        for key in keys {
            if entry.seen.iter().any(|s| s.key == key) {
                continue;
            }
            entry.seen.push(SeenEntry { key, ts: now });
        }
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
