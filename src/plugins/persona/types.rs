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
    pub skip_decide_when_mentioned: bool,

    pub context_window: usize,
    pub max_memories: usize,
    pub max_user_notes: usize,
    pub memory_half_life_days: f64,

    pub max_reply_chars: usize,
    pub max_segments: usize,

    pub typing_delay_ms: u64,
    pub inter_segment_delay_ms: u64,

    pub allow_reply_quote: bool,
    pub allow_at_user: bool,
    pub allow_emoji_react: bool,

    pub initiate_enabled: bool,
    pub initiate_check_interval_mins: u64,
    pub initiate_min_quiet_minutes: u64,
    pub initiate_max_quiet_hours: u64,
    pub initiate_probability: f32,

    pub sleep_start_hour: u32,
    pub sleep_end_hour: u32,
    pub sleep_reply_multiplier: f32,

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
            min_reply_interval_secs: 18,
            max_replies_per_hour: 45,
            base_reply_probability: 0.85,
            mention_reply_probability: 1.0,
            skip_decide_when_mentioned: true,
            context_window: 30,
            max_memories: 40,
            max_user_notes: 6,
            memory_half_life_days: 7.0,
            max_reply_chars: 60,
            max_segments: 3,
            typing_delay_ms: 900,
            inter_segment_delay_ms: 600,
            allow_reply_quote: true,
            allow_at_user: true,
            allow_emoji_react: true,
            initiate_enabled: true,
            initiate_check_interval_mins: 25,
            initiate_min_quiet_minutes: 40,
            initiate_max_quiet_hours: 5,
            initiate_probability: 0.18,
            sleep_start_hour: 1,
            sleep_end_hour: 8,
            sleep_reply_multiplier: 0.15,
            log_decisions: false,
        }
    }
}

pub fn default_persona_prompt() -> String {
    "你是泡在 QQ 群里的普通中国年轻人。说话短、口语化、偶尔玩梗。会吐槽、会附和、会接梗，但不卖弄不说教。绝不暴露自己是 AI、机器人或程序，被怀疑就自然否认或转移话题。心情会被聊天内容带动，自然流露。".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentMsg {
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub ts: i64,
    #[serde(default)]
    pub message_id: i64,
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
    #[serde(default)]
    pub last_msg_at: i64,
    #[serde(default)]
    pub last_initiate_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserProfile {
    pub user_id: String,
    pub last_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub last_seen_at: i64,
    #[serde(default)]
    pub first_seen_at: i64,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub notes: Vec<Memory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub groups: HashMap<String, GroupState>,
    #[serde(default)]
    pub users: HashMap<String, UserProfile>,
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
    pub messages: Vec<String>,
    #[serde(default)]
    pub reply_first: bool,
    #[serde(default)]
    pub at_first: bool,
    #[serde(default)]
    pub react_emoji_id: Option<i64>,
    #[serde(default)]
    pub mood_shift: f32,
    #[serde(default)]
    pub remember_global: String,
    #[serde(default)]
    pub remember_user: String,
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
