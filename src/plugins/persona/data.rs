use super::memory::now_secs;
use super::types::{RecentMsg, State, UserProfile};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

pub static MANAGER: OnceLock<Arc<Manager>> = OnceLock::new();

pub struct Manager {
    pub state: RwLock<State>,
    pub in_flight: RwLock<HashSet<String>>,
    pub path: PathBuf,
}

impl Manager {
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("state.json");
        let state = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<State>(&s).ok())
                .unwrap_or_default()
        } else {
            State::default()
        };
        Self {
            state: RwLock::new(state),
            in_flight: RwLock::new(HashSet::new()),
            path,
        }
    }

    pub async fn save(&self) {
        let snap = self.state.read().await.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(s) = serde_json::to_string(&snap) {
                let _ = std::fs::write(&path, s);
            }
        });
    }

    pub async fn try_acquire(&self, group_key: &str) -> bool {
        let mut g = self.in_flight.write().await;
        if g.contains(group_key) {
            return false;
        }
        g.insert(group_key.to_string());
        true
    }

    pub async fn release(&self, group_key: &str) {
        self.in_flight.write().await.remove(group_key);
    }

    pub async fn append_recent(&self, group_key: &str, msg: RecentMsg, context_window: usize) {
        let mut s = self.state.write().await;
        let g = s.groups.entry(group_key.to_string()).or_default();
        g.last_msg_at = msg.ts;
        g.recent.push(msg);
        if g.recent.len() > context_window {
            let drop = g.recent.len() - context_window;
            g.recent.drain(0..drop);
        }
    }

    pub async fn touch_user(&self, user_id: &str, current_name: &str) {
        if user_id.is_empty() {
            return;
        }
        let now = now_secs();
        let mut s = self.state.write().await;
        let prof = s
            .users
            .entry(user_id.to_string())
            .or_insert_with(|| UserProfile {
                user_id: user_id.to_string(),
                last_name: current_name.to_string(),
                first_seen_at: now,
                last_seen_at: now,
                ..Default::default()
            });

        if prof.first_seen_at == 0 {
            prof.first_seen_at = now;
        }
        prof.last_seen_at = now;
        prof.message_count = prof.message_count.saturating_add(1);

        if !current_name.is_empty() && current_name != prof.last_name {
            // 新名字进 last_name；旧名字塞 alias
            if !prof.last_name.is_empty()
                && prof.last_name != current_name
                && !prof.aliases.iter().any(|a| a == &prof.last_name)
            {
                prof.aliases.push(prof.last_name.clone());
                if prof.aliases.len() > 5 {
                    let drop = prof.aliases.len() - 5;
                    prof.aliases.drain(0..drop);
                }
            }
            prof.last_name = current_name.to_string();
        }
    }

    pub async fn cooldown_ok(
        &self,
        group_key: &str,
        min_interval_secs: u64,
        max_per_hour: u32,
    ) -> bool {
        let s = self.state.read().await;
        let Some(g) = s.groups.get(group_key) else {
            return true;
        };
        let now = now_secs();
        if (now - g.last_reply_at) < min_interval_secs as i64 {
            return false;
        }
        let recent_count = g
            .reply_history
            .iter()
            .rev()
            .take_while(|t| now - **t < 3600)
            .count() as u32;
        if recent_count >= max_per_hour {
            return false;
        }
        true
    }

    pub async fn record_reply(&self, group_key: &str) {
        let mut s = self.state.write().await;
        let g = s.groups.entry(group_key.to_string()).or_default();
        let now = now_secs();
        g.last_reply_at = now;
        g.reply_history.push(now);
        let one_hour_ago = now - 3600;
        g.reply_history.retain(|t| *t > one_hour_ago);
    }
}
