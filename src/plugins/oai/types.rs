use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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

/// 房间交给本机 pi agent 执行。
pub const ENGINE_PI: &str = "pi";
/// 房间走中转站的 Chat Completions（含 MJ / 图像房间）。
pub const ENGINE_CHAT: &str = "chat";

/// 合法的思考强度档位，与 Pi 的 `--thinking` 取值一致（普通房间转成 `reasoning_effort`）。
pub const THINKING_LEVELS: [&str; 5] = ["off", "minimal", "low", "medium", "high"];

/// 归一化思考强度：只认内置档位，大小写与首尾空白不敏感；其余返回 `None`。
pub fn normalize_thinking(value: &str) -> Option<String> {
    let level = value.trim().to_ascii_lowercase();
    THINKING_LEVELS.contains(&level.as_str()).then_some(level)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 房间在 `/#` 列表里归到哪个分区；留空则按模型分组（一直以来的样子）。
    ///
    /// 内置的画图预设用它单独成区：那一批房间的共同点是「预设」而不是「模型」，
    /// 按模型分组会把它们混进用户自己建的同模型房间里。
    #[serde(default)]
    pub section: String,
    /// 执行引擎：[`ENGINE_PI`] 或 [`ENGINE_CHAT`]。
    ///
    /// 房间名曾经是唯一的开关——只有 `pi` 和 `pi-*` 能用 pi agent，于是所有想用 pi
    /// 的房间都被迫顶着这个前缀。现在引擎是房间自己的属性，名字随便取。
    /// 留空表示还没迁移过的旧配置，此时仍按旧的名字规则推断。
    #[serde(default)]
    pub engine: String,
    /// 引擎对应的模型：中转站房间是 `供应商/模型` 或裸模型 id；pi 房间是 pi 的
    /// `--model`（`provider/id` 或裸 id），留空或 `pi` 表示沿用 pi 自身配置。
    ///
    /// 中转站房间的 `供应商/` 前缀决定打到哪个接口（见 `[oai.providers]`），
    /// 不带前缀时沿用 `oai` 默认接口。
    pub model: String,
    /// 房间默认思考强度（`off` / `minimal` / `low` / `medium` / `high`）。
    /// 留空表示交给引擎默认；模型写法里的 `:强度` 后缀优先于这里。
    #[serde(default)]
    pub thinking: String,
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
            section: String::new(),
            engine: String::new(),
            model: model.to_string(),
            thinking: String::new(),
            system_prompt: prompt.to_string(),
            public_history: Vec::new(),
            private_histories: HashMap::new(),
            generation_id: 0,
            created_at: chrono::Local::now().timestamp(),
        }
    }

    /// 这个房间是否由本机 pi agent 接管。
    ///
    /// 显式的 `engine` 说了算；只有还没迁移过的旧配置才回退到「名字叫 pi 或 pi-*」。
    pub fn uses_pi(&self) -> bool {
        match self.engine.trim() {
            "" => super::agent::legacy_pi_name(&self.name),
            engine => engine.eq_ignore_ascii_case(ENGINE_PI),
        }
    }

    /// 记下这个房间用哪个引擎和模型；引擎一旦写下就不再依赖房间名。
    pub fn set_engine(&mut self, engine: &str, model: &str) {
        self.engine = engine.to_string();
        self.model = model.to_string();
    }

    /// 请求时真正生效的思考强度：模型写法里的 `:强度` 后缀优先，其次房间字段。
    ///
    /// `set_engine` 会把后缀收进 `thinking`，所以两者通常一致；保留后缀优先是因为
    /// 旧配置或手改的模型串可能带着后缀（Pi 的 `id:thinking` 写法），不该被忽略。
    pub fn effective_thinking(&self) -> Option<String> {
        super::utils::split_thinking(&self.model)
            .1
            .or_else(|| normalize_thinking(&self.thinking))
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
    /// 记录内置 `pi` 房间已经迁移过，用户主动删除后不会在每次启动时复活。
    #[serde(default)]
    pub pi_room_initialized: bool,
    /// 内置默认值迁移版本；避免每次启动覆盖管理员后续的模型选择。
    #[serde(default)]
    pub defaults_version: u32,
    /// 已经建过的内置预设房间名（见 [`super::presets`]）。
    ///
    /// 记的是「建过」而不是「存在」：管理员删掉哪间就是不要哪间，下次启动不复活；
    /// 而新加的预设仍会补建，因为它的名字还不在这张表里。
    #[serde(default)]
    pub seeded_presets: Vec<String>,
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
    pub public: HashMap<String, u64>,
    pub private: HashMap<String, HashMap<String, u64>>,
}

impl GeneratingState {
    pub fn is_generating(&self, agent: &str, private: bool, uid: &str) -> bool {
        self.current(agent, private, uid).is_some()
    }
    fn current(&self, agent: &str, private: bool, uid: &str) -> Option<u64> {
        if private {
            self.private.get(agent)?.get(uid).copied()
        } else {
            self.public.get(agent).copied()
        }
    }
    pub fn is_current(&self, agent: &str, private: bool, uid: &str, id: u64) -> bool {
        self.current(agent, private, uid) == Some(id)
    }
    /// 在同一个写锁内占用会话，避免两个请求同时通过空闲检查。
    pub fn begin(&mut self, agent: &str, private: bool, uid: &str) -> Option<u64> {
        if self.is_generating(agent, private, uid) {
            return None;
        }
        let id = rand::random();
        if private {
            self.private
                .entry(agent.to_string())
                .or_default()
                .insert(uid.to_string(), id);
        } else {
            self.public.insert(agent.to_string(), id);
        }
        Some(id)
    }
    pub fn cancel_room(&mut self, agent: &str) {
        self.public.remove(agent);
        self.private.remove(agent);
    }
    pub fn set_generating(&mut self, agent: &str, private: bool, uid: &str, generating: bool) {
        if generating {
            self.begin(agent, private, uid);
        } else if private {
            if let Some(users) = self.private.get_mut(agent) {
                users.remove(uid);
            }
        } else {
            self.public.remove(agent);
        }
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn any_room_name_can_run_pi_once_the_engine_is_written_down() {
        let mut room = Agent::new("研究", "gpt-5.6-luna", "", "");
        assert!(!room.uses_pi(), "名字普通、引擎未定的房间仍走中转站");
        room.set_engine(ENGINE_PI, "apilio/claude-opus-5");
        assert!(room.uses_pi(), "名字没变，引擎说了算");
        assert_eq!(room.model, "apilio/claude-opus-5");
        room.set_engine(ENGINE_CHAT, "gpt-5.6-luna");
        assert!(!room.uses_pi(), "换回中转站不需要改名");
    }

    #[test]
    fn thinking_comes_from_the_model_suffix_first_then_the_room_field() {
        let mut room = Agent::new("助手", "deepseek/deepseek-flash", "", "");
        assert_eq!(room.effective_thinking(), None);
        room.thinking = "low".into();
        assert_eq!(room.effective_thinking().as_deref(), Some("low"));
        // 模型串里的 `:强度` 优先于房间字段（旧配置/手改模型串的情形）。
        room.model = "deepseek/deepseek-flash:high".into();
        assert_eq!(room.effective_thinking().as_deref(), Some("high"));
        // 房间字段非法档位直接忽略。
        room.model = "deepseek/deepseek-flash".into();
        room.thinking = "unlimited".into();
        assert_eq!(room.effective_thinking(), None);
    }

    /// 还没迁移过的配置里 `engine` 是空的；那时仍按当初的名字规则判断，
    /// 已有的 `pi` / `pi-*` 房间不会在升级的一瞬间变成普通房间。
    #[test]
    fn legacy_configs_without_an_engine_field_keep_their_old_behaviour() {
        let legacy: Agent = serde_json::from_str(
            r#"{"name":"pi-猫娘","model":"gpt-5.6-luna","system_prompt":""}"#,
        )
        .unwrap();
        assert!(legacy.engine.is_empty());
        assert!(legacy.uses_pi());
        let ordinary: Agent =
            serde_json::from_str(r#"{"name":"助手","model":"gpt-5.6-luna","system_prompt":""}"#)
                .unwrap();
        assert!(!ordinary.uses_pi());
        // 显式引擎优先于名字：叫 pi 也能被改回中转站房间。
        let mut renamed = legacy.clone();
        renamed.set_engine(ENGINE_CHAT, "gpt-5.6-luna");
        assert!(!renamed.uses_pi());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn cancellation_and_concurrent_histories_are_independent() {
        let mut state = GeneratingState::default();
        let public = state.begin("pi", false, "alice").unwrap();
        let alice = state.begin("pi", true, "alice").unwrap();
        let bob = state.begin("pi", true, "bob").unwrap();
        assert!(state.begin("pi", false, "bob").is_none());
        state.set_generating("pi", true, "alice", false);
        assert!(!state.is_current("pi", true, "alice", alice));
        assert!(state.is_current("pi", true, "bob", bob));
        assert!(state.is_current("pi", false, "alice", public));
        let next = state.begin("pi", true, "alice").unwrap();
        assert!(state.is_current("pi", true, "alice", next));
        assert!(!state.is_current("pi", true, "alice", alice));
    }
}

/// 正文引用到的网页来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub title: String,
    pub url: String,
}

/// 工具调用轨迹里的一步。
///
/// 页脚按结构渲染而不是拼成一行长文本：一行文本迟早要被字数截断，
/// 而工具参数（命令、搜索词、URL）恰恰是尾巴最有信息量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceStep {
    /// 工具名，例如 `bash`。
    pub name: String,
    /// 参数摘要；可能为空。
    pub detail: String,
    /// 连续同名同参调用的合并次数，至少为 1。
    pub repeats: u32,
}

impl TraceStep {
    pub(crate) fn new(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            detail: detail.into(),
            repeats: 1,
        }
    }
}
