use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::config::build_config;
use crate::event::{Context, EventType};
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::derived::ValueObjectAccess;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use toml::Value as TomlValue;

// ================= 配置定义 =================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepeaterConfig {
    pub min_times: usize,
    pub probability: f64,
}

impl Default for RepeaterConfig {
    fn default() -> Self {
        Self {
            min_times: 2,
            probability: 1.0,
        }
    }
}

pub fn default_config() -> TomlValue {
    build_config(RepeaterConfig::default())
}

// ================= 状态定义 =================

#[derive(Debug, Default, Clone)]
struct ChannelState {
    content: OwnedValue,
    repeated: bool,
    times: usize,
    last_user_id: String,
    last_active: u64,
}

static STATES: OnceLock<Mutex<HashMap<String, ChannelState>>> = OnceLock::new();

fn get_state_lock() -> &'static Mutex<HashMap<String, ChannelState>> {
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

// 长期运行的 Bot 中频道数量可能膨胀。定期清理旧条目以防内存泄漏：
// - 触发条件：HashMap 大小超过 MAX_CHANNELS
// - 策略：移除空闲时长超过 IDLE_TIMEOUT_SECS 的频道
const MAX_CHANNELS: usize = 4096;
const IDLE_TIMEOUT_SECS: u64 = 60 * 60; // 1 小时

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn maybe_evict(map: &mut HashMap<String, ChannelState>, now: u64) {
    if map.len() <= MAX_CHANNELS {
        return;
    }
    map.retain(|_, st| now.saturating_sub(st.last_active) < IDLE_TIMEOUT_SECS);
}

// ================= 逻辑实现 =================

fn check_probability(probability: f64) -> bool {
    if probability >= 1.0 {
        return true;
    }
    if probability <= 0.0 {
        return false;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let random_val = (nanos % 100) as f64 / 100.0;
    random_val < probability
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let config: RepeaterConfig = get_config(&ctx, "repeater").unwrap_or_default();

        // === 场景 A: 接收到新消息 (OneBot Message) ===
        if let Some(msg) = ctx.as_message() {
            let channel_id = if let Some(gid) = msg.group_id() {
                gid.to_string()
            } else if msg.user_id() != 0 {
                msg.user_id().to_string()
            } else {
                return Ok(Some(ctx));
            };

            let content = if let EventType::Onebot(ev) = &ctx.event {
                ev.get("message")
                    .cloned()
                    .unwrap_or_else(|| OwnedValue::from(Vec::<OwnedValue>::new()))
            } else {
                OwnedValue::from(Vec::<OwnedValue>::new())
            };

            let sender_id = msg.user_id().to_string();

            let should_repeat = {
                let mut states = get_state_lock().lock().unwrap();
                let now = now_secs();
                maybe_evict(&mut states, now);
                let state = states.entry(channel_id.clone()).or_default();
                state.last_active = now;

                if state.content == content {
                    // 只有不同人发送相同内容才计数
                    if state.last_user_id != sender_id {
                        state.times += 1;
                        state.last_user_id = sender_id;

                        if state.times >= config.min_times
                            && !state.repeated
                            && check_probability(config.probability)
                        {
                            state.repeated = true;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    // 内容变了，重置状态
                    state.content = content.clone();
                    state.times = 1;
                    state.repeated = false;
                    state.last_user_id = sender_id;
                    false
                }
            };

            if should_repeat {
                debug!(target: "Plugin/Repeater", "触发复读");
                let group_id = msg.group_id();
                send_msg(&ctx, writer, group_id, None, content).await?;
            }
        }
        // === 场景 B: 消息发送前 (Before Send) ===
        else if let EventType::BeforeSend(packet) = &ctx.event
            && let Some(gid) = packet.group_id()
        {
            let channel_id = gid.to_string();

            let content = packet
                .message()
                .cloned()
                .unwrap_or_else(|| OwnedValue::from(Vec::<OwnedValue>::new()));

            let bot_marker = "<BOT_SELF>".to_string();

            let mut states = get_state_lock().lock().unwrap();
            let now = now_secs();
            maybe_evict(&mut states, now);
            let state = states.entry(channel_id).or_default();

            state.repeated = true;
            state.last_active = now;

            if state.content == content {
                if state.last_user_id != bot_marker {
                    state.times += 1;
                    state.last_user_id = bot_marker;
                }
            } else {
                state.content = content;
                state.times = 1;
                state.last_user_id = bot_marker;
            }
        }

        Ok(Some(ctx))
    })
}
