use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaConfig {
    pub enabled: bool,

    pub api_base: String,
    pub api_key: String,

    pub decide_model: String,
    pub reply_model: String,

    pub nickname: String,
    pub persona_prompt: String,

    pub allow_groups: Vec<i64>,

    pub min_reply_interval_secs: u64,
    pub max_replies_per_hour: u32,

    pub base_reply_probability: f32,
    pub mention_reply_probability: f32,

    pub context_window: usize,
    pub max_memories: usize,
    pub memory_half_life_days: f64,

    pub max_reply_chars: usize,
    pub typing_delay_ms: u64,

    pub log_decisions: bool,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_base: "https://api.deepseek.com".to_string(),
            api_key: String::new(),
            decide_model: "deepseek-v4-flash".to_string(),
            reply_model: "deepseek-v4-flash".to_string(),
            nickname: "小阿".to_string(),
            persona_prompt: default_persona_prompt(),
            allow_groups: Vec::new(),
            min_reply_interval_secs: 25,
            max_replies_per_hour: 30,
            base_reply_probability: 0.55,
            mention_reply_probability: 0.95,
            context_window: 30,
            max_memories: 40,
            memory_half_life_days: 7.0,
            max_reply_chars: 80,
            typing_delay_ms: 1200,
            log_decisions: false,
        }
    }
}

pub fn default_persona_prompt() -> String {
    "你是一个泡在 QQ 群里的普通中国年轻人。说话短、口语、偶尔网络梗，会吐槽也会附和，不卖弄不说教，绝不暴露自己是 AI 或机器人。心情会被聊天内容影响，自然流露出来就行。".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMsg {
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub ts: i64,
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub mentions_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub content: String,
    pub importance: f32,
    pub created_at: i64,
    pub last_recalled_at: i64,
    #[serde(default)]
    pub recall_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GroupState {
    #[serde(default)]
    pub mood: f32,
    #[serde(default)]
    pub recent: Vec<RecentMsg>,
    #[serde(default)]
    pub memories: Vec<Memory>,
    #[serde(default)]
    pub last_reply_at: i64,
    #[serde(default)]
    pub reply_history: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub groups: HashMap<String, GroupState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecideResult {
    #[serde(default)]
    pub reply: bool,
    #[serde(default)]
    pub urgency: u8,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReplyResult {
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub mood_shift: f32,
    #[serde(default)]
    pub remember: String,
}

pub fn mood_label(m: f32) -> &'static str {
    if m > 0.5 {
        "挺开心的"
    } else if m > 0.15 {
        "心情还不错"
    } else if m > -0.15 {
        "平静"
    } else if m > -0.5 {
        "有点烦"
    } else {
        "心情很差"
    }
}
