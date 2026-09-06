use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub timestamp: i64,
}

impl ChatMessage {
    pub fn new(role: &str, content: &str, images: Vec<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
            images,
            timestamp: chrono::Local::now().timestamp(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub public_history: Vec<ChatMessage>,
    #[serde(default)]
    pub private_histories: HashMap<String, Vec<ChatMessage>>,
    #[serde(default)]
    pub generation_id: u64,
    #[serde(default)]
    pub created_at: i64,
}

impl Agent {
    pub fn new(name: &str, model: &str, prompt: &str, desc: &str) -> Self {
        Self {
            name: name.to_string(),
            description: desc.to_string(),
            model: model.to_string(),
            system_prompt: prompt.to_string(),
            public_history: Vec::new(),
            private_histories: HashMap::new(),
            generation_id: 0,
            created_at: chrono::Local::now().timestamp(),
        }
    }

    pub fn history_mut(&mut self, private: bool, uid: &str) -> &mut Vec<ChatMessage> {
        if private {
            self.private_histories.entry(uid.to_string()).or_default()
        } else {
            &mut self.public_history
        }
    }

    pub fn history(&self, private: bool, uid: &str) -> &[ChatMessage] {
        if private {
            self.private_histories
                .get(uid)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        } else {
            &self.public_history
        }
    }

    pub fn clear_history(&mut self, private: bool, uid: &str) {
        if private {
            if let Some(h) = self.private_histories.get_mut(uid) {
                h.clear();
            }
        } else {
            self.public_history.clear();
        }
    }

    pub fn delete_at(&mut self, private: bool, uid: &str, indices: &[usize]) -> Vec<usize> {
        let h = self.history_mut(private, uid);
        let mut deleted = Vec::new();
        let mut sorted: Vec<usize> = indices.to_vec();
        sorted.sort_by(|a, b| b.cmp(a));
        sorted.dedup();
        for i in sorted {
            if i > 0 && i <= h.len() {
                h.remove(i - 1);
                deleted.push(i);
            }
        }
        deleted.reverse();
        deleted
    }

    pub fn edit_at(&mut self, private: bool, uid: &str, idx: usize, content: &str) -> bool {
        let h = self.history_mut(private, uid);
        if idx > 0 && idx <= h.len() {
            h[idx - 1].content = content.to_string();
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub api_base: String,
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub default_prompt: String,
    /// 记录内置 `pi` 房间已经迁移过，用户主动删除后不会在每次启动时复活。
    #[serde(default)]
    pub pi_room_initialized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MjMessageTask {
    pub task_id: String,
    /// 任务实际使用的接入点（`mj-fast` / `mj-relax`），旧缓存缺省时走当前默认值。
    #[serde(default)]
    pub api_base: String,
    #[serde(default)]
    pub upscale_buttons: HashMap<u8, String>,
    #[serde(default)]
    pub created_at: i64,
}

/// MJ 的引用关系和放大结果独立于聊天记录持久化。
/// `upscales` 的值优先是本地缓存文件，下载失败时回退为远程图片 URL。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MjCache {
    #[serde(default)]
    pub messages: HashMap<String, MjMessageTask>,
    #[serde(default)]
    pub upscales: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct GeneratingState {
    pub public: HashSet<String>,
    pub private: HashMap<String, HashSet<String>>,
}

impl GeneratingState {
    pub fn is_generating(&self, agent: &str, private: bool, uid: &str) -> bool {
        if private {
            self.private
                .get(agent)
                .map(|s| s.contains(uid))
                .unwrap_or(false)
        } else {
            self.public.contains(agent)
        }
    }

    pub fn set_generating(&mut self, agent: &str, private: bool, uid: &str, generating: bool) {
        if private {
            let set = self.private.entry(agent.to_string()).or_default();
            if generating {
                set.insert(uid.to_string());
            } else {
                set.remove(uid);
            }
        } else if generating {
            self.public.insert(agent.to_string());
        } else {
            self.public.remove(agent);
        }
    }
}
