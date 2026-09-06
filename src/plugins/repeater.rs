//! 复读机：同一句话在频道里被接力说到阈值时，机器人跟读一次。
//!
//! 判定不看原始消息 JSON，而是先抽一条可比较的**指纹**：文本折叠空白，
//! 图片/表情取资源 ID。同一张图换个带 token 的链接再发，仍算同一句话。
//!
//! 有些消息不该跟读，指纹阶段直接判定为不可复读并**打断当前接力**：
//!
//!   - 带 `@`、引用、转发、文件、卡片的消息——跟读等于机器人替人 at，
//!     或者刷出一张没有上下文的卡片；
//!   - 指令消息（`ignore_commands`）——两个人发 `/help`，机器人不必跟着刷；
//!   - 超过 `max_chars` 的长文——复读长文是纯刷屏。
//!
//! Bot 自己说的话（`BeforeSend` 拦截到的发送包，以及实现端回显的自身消息）
//! 只更新状态、不参与计数，避免自己接自己的话形成连锁。
//!
//! 触发点上依次过三道闸：冷却 → 概率 → 打断。命中打断则改发一句打断语，
//! 概率未命中不置位 `repeated`，同一句话的下一条仍有机会触发。

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::get_prefixes;
use crate::config::build_config;
use crate::event::{Context, EventType};
use crate::message::Message;
use crate::plugins::{PluginError, get_config_or_default};
use futures_util::future::BoxFuture;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;
use simd_json::base::ValueAsArray;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use toml::Value as TomlValue;

const LOG_TARGET: &str = "Plugin/Repeater";

// ================= 配置定义 =================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelConfig {
    /// 白名单：非空时只在这些群生效
    pub white: Vec<i64>,
    /// 黑名单：这些群一律不复读
    pub black: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RepeaterConfig {
    pub enabled: bool,
    /// 同一句话累计到几条开始跟读（小于 2 按 2 处理，否则等于逢消息必复读）
    pub min_times: usize,
    /// 触发概率，取值 0~1
    pub probability: f64,
    /// 同一频道两次复读之间的冷却秒数，0 为不限制
    pub cooldown_seconds: u64,
    /// 参与判定的文本长度上限，超长不复读；0 为不限制
    pub max_chars: usize,
    /// 是否允许同一个人自己刷屏凑够阈值
    pub allow_same_user: bool,
    /// 是否跳过指令消息
    pub ignore_commands: bool,
    /// 是否允许复读图片与表情
    pub allow_media: bool,
    /// 打断复读的概率：在本该跟读时改为发送一句打断语
    pub interrupt_probability: f64,
    /// 打断语文案池，随机取一条
    pub interrupt_texts: Vec<String>,
    /// 群黑白名单
    pub channel: ChannelConfig,
}

impl Default for RepeaterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_times: 2,
            probability: 1.0,
            cooldown_seconds: 15,
            max_chars: 200,
            allow_same_user: false,
            ignore_commands: true,
            allow_media: true,
            interrupt_probability: 0.0,
            interrupt_texts: vec!["打断复读".to_string(), "打断施法".to_string()],
            channel: ChannelConfig::default(),
        }
    }
}

pub fn default_config() -> TomlValue {
    build_config(RepeaterConfig::default())
}

impl RepeaterConfig {
    /// min_times = 0/1 等价于逢消息必复读，收敛到 2
    fn threshold(&self) -> usize {
        self.min_times.max(2)
    }
}

// ================= 状态定义 =================

/// 一条消息的来源：参与计数的人，或不参与计数的 Bot 自身
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sender {
    User(i64),
    Bot,
}

#[derive(Debug, Default, Clone)]
struct ChannelState {
    /// 当前接力中的消息指纹，空串表示接力已被打断
    sig: String,
    /// 复读时原样发出的消息内容
    content: OwnedValue,
    /// 当前指纹已累计的条数
    times: usize,
    /// 本轮是否已经跟读过
    repeated: bool,
    /// 上一条消息的来源
    last_sender: Option<Sender>,
    /// 上次实际复读的时间戳（跨轮保留，用于冷却）
    last_repeat_at: u64,
    last_active: u64,
}

impl ChannelState {
    /// 换了一句话：重置接力，但保留冷却与活跃时间
    fn restart(&mut self, sig: String, content: OwnedValue, sender: Sender) {
        self.sig = sig;
        self.content = content;
        self.times = if sender == Sender::Bot { 0 } else { 1 };
        // Bot 自己刚说的话不该被自己跟读
        self.repeated = sender == Sender::Bot;
        self.last_sender = Some(sender);
    }

    /// 打断接力：下一条消息一律从头开始数
    fn interrupt_chain(&mut self) {
        self.sig.clear();
        self.times = 0;
        self.repeated = false;
        self.last_sender = None;
    }
}

static STATES: OnceLock<Mutex<HashMap<String, ChannelState>>> = OnceLock::new();

/// 取状态表。锁中毒说明此前某次持锁 panic 过，状态本身仍可用，不再连坐 panic。
fn states() -> MutexGuard<'static, HashMap<String, ChannelState>> {
    STATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

// 长期运行的 Bot 中频道数量可能膨胀。超过上限时先清理空闲频道，
// 仍超限则按最后活跃时间淘汰最旧的，保证表长有界。
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
    if map.len() <= MAX_CHANNELS {
        return;
    }
    let mut by_age: Vec<(u64, String)> = map
        .iter()
        .map(|(key, st)| (st.last_active, key.clone()))
        .collect();
    by_age.sort_unstable_by_key(|(active, _)| *active);
    for (_, key) in by_age.into_iter().take(map.len() - MAX_CHANNELS) {
        map.remove(&key);
    }
}

// ================= 指纹 =================

/// 折叠空白：换行、空格数量不同的同一句话仍算同一句
fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// 取媒体段的稳定标识：优先资源 ID，退而求其次用去掉查询串的链接
fn media_key(type_: &str, data: &OwnedValue) -> Option<String> {
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| {
                data.get_str(*key)
                    .map(str::to_string)
                    .or_else(|| data.get_i64(*key).map(|v| v.to_string()))
                    .or_else(|| data.get_u64(*key).map(|v| v.to_string()))
            })
            .filter(|value| !value.is_empty())
    };
    match type_ {
        "image" => pick(&["file", "md5", "file_unique", "file_id"])
            .or_else(|| pick(&["url"]).map(|url| strip_query(&url).to_string()))
            .map(|key| format!("i:{key}")),
        "face" => pick(&["id"]).map(|key| format!("f:{key}")),
        "mface" => pick(&["emoji_id", "summary", "key", "id"]).map(|key| format!("m:{key}")),
        _ => None,
    }
}

/// 消息指纹。返回 None 表示这条消息不可复读，当前接力就此打断。
fn signature(message: &[OwnedValue], config: &RepeaterConfig) -> Option<String> {
    let mut parts: Vec<String> = Vec::with_capacity(message.len());
    let mut chars = 0usize;

    for segment in message {
        let type_ = segment.get_str("type")?;
        let data = segment.get("data");
        match type_ {
            "text" => {
                let text = normalize_text(data.and_then(|d| d.get_str("text")).unwrap_or(""));
                if text.is_empty() {
                    continue;
                }
                chars += text.chars().count();
                parts.push(format!("t:{text}"));
            }
            "image" | "face" | "mface" if config.allow_media => {
                parts.push(media_key(type_, data?)?);
            }
            // at / reply / forward / file / json / poke ……跟读它们只会误伤
            _ => return None,
        }
    }

    if parts.is_empty() {
        return None;
    }
    if config.max_chars > 0 && chars > config.max_chars {
        return None;
    }
    Some(parts.join("|"))
}

fn is_command(prefixes: &[String], text: &str) -> bool {
    let text = text.trim_start();
    prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && text.starts_with(prefix.as_str()))
}

fn channel_key(bot_id: &str, group_id: Option<i64>, user_id: i64) -> Option<String> {
    match group_id {
        Some(gid) => Some(format!("{bot_id}#g{gid}")),
        None if user_id != 0 => Some(format!("{bot_id}#p{user_id}")),
        _ => None,
    }
}

fn allow_channel(group_id: Option<i64>, config: &ChannelConfig) -> bool {
    let Some(gid) = group_id else {
        return true; // 私聊不受群名单约束
    };
    if config.black.contains(&gid) {
        return false;
    }
    config.white.is_empty() || config.white.contains(&gid)
}

// ================= 触发判定 =================

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// 只更新状态
    Silent,
    /// 原样跟读
    Repeat,
    /// 打断复读
    Interrupt(String),
}

fn roll(probability: f64) -> bool {
    if probability >= 1.0 {
        return true;
    }
    if probability <= 0.0 {
        return false;
    }
    rand::rng().random_bool(probability)
}

fn pick_interrupt(texts: &[String]) -> Option<String> {
    let candidates: Vec<&String> = texts.iter().filter(|t| !t.trim().is_empty()).collect();
    match candidates.len() {
        0 => None,
        1 => Some(candidates[0].clone()),
        n => Some(candidates[rand::rng().random_range(0..n)].clone()),
    }
}

/// 把一条消息喂给频道状态，返回该做什么。纯逻辑，便于单测。
fn feed(
    state: &mut ChannelState,
    sig: String,
    content: OwnedValue,
    sender: Sender,
    config: &RepeaterConfig,
    now: u64,
) -> Action {
    state.last_active = now;

    if state.sig != sig {
        state.restart(sig, content, sender);
        return Action::Silent;
    }

    // Bot 自己重复了这句话：只压住后续跟读，不计数
    if sender == Sender::Bot {
        state.repeated = true;
        state.last_sender = Some(Sender::Bot);
        return Action::Silent;
    }

    // 同一个人连着刷，默认不算接力
    if !config.allow_same_user && state.last_sender.as_ref() == Some(&sender) {
        return Action::Silent;
    }

    state.times += 1;
    state.last_sender = Some(sender);

    if state.repeated || state.times < config.threshold() {
        return Action::Silent;
    }
    if config.cooldown_seconds > 0
        && now.saturating_sub(state.last_repeat_at) < config.cooldown_seconds
    {
        return Action::Silent;
    }
    // 概率没命中不置位，同一句话的下一条还能再摇一次
    if !roll(config.probability) {
        return Action::Silent;
    }

    state.repeated = true;
    state.last_repeat_at = now;

    if roll(config.interrupt_probability)
        && let Some(text) = pick_interrupt(&config.interrupt_texts)
    {
        return Action::Interrupt(text);
    }
    Action::Repeat
}

// ================= 逻辑实现 =================

/// 不可复读的消息：打断接力后返回
fn break_chain(key: String, now: u64) {
    let mut map = states();
    maybe_evict(&mut map, now);
    let state = map.entry(key).or_default();
    state.last_active = now;
    state.interrupt_chain();
}

fn observe(
    key: String,
    sig: String,
    content: OwnedValue,
    sender: Sender,
    config: &RepeaterConfig,
    now: u64,
) -> Action {
    let mut map = states();
    maybe_evict(&mut map, now);
    let state = map.entry(key).or_default();
    feed(state, sig, content, sender, config, now)
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let config: RepeaterConfig = get_config_or_default(&ctx, "repeater");
        let now = now_secs();

        // === 场景 A: 收到消息 ===
        if let Some(msg) = ctx.as_message() {
            let group_id = msg.group_id().filter(|id| *id != 0);
            if !allow_channel(group_id, &config.channel) {
                return Ok(Some(ctx));
            }

            let user_id = msg.user_id();
            let Some(key) = channel_key(&ctx.bot.login_user.id, group_id, user_id) else {
                return Ok(Some(ctx));
            };
            let EventType::Satori(event) = &ctx.event else {
                return Ok(Some(ctx));
            };
            let Some(segments) = event.get_array("message") else {
                break_chain(key, now);
                return Ok(Some(ctx));
            };

            if config.ignore_commands && is_command(&get_prefixes(&ctx), msg.text()) {
                break_chain(key, now);
                return Ok(Some(ctx));
            }
            let Some(sig) = signature(segments, &config) else {
                break_chain(key, now);
                return Ok(Some(ctx));
            };

            // 实现端回显的自身消息与 BeforeSend 走同一条路径，避免重复计数
            let self_id = ctx.bot.login_user.id.parse::<i64>().unwrap_or(0);
            let sender = if user_id != 0 && user_id == self_id {
                Sender::Bot
            } else {
                Sender::User(user_id)
            };

            let content = event
                .get("message")
                .cloned()
                .unwrap_or_else(|| OwnedValue::from(Vec::<OwnedValue>::new()));

            match observe(key, sig, content.clone(), sender, &config, now) {
                Action::Silent => {}
                Action::Repeat => {
                    debug!(target: LOG_TARGET, "触发复读");
                    send_msg(&ctx, writer, group_id, Some(user_id), content).await?;
                }
                Action::Interrupt(text) => {
                    debug!(target: LOG_TARGET, "打断复读: {}", text);
                    let message = Message::new().text(text);
                    send_msg(&ctx, writer, group_id, Some(user_id), message).await?;
                }
            }
        }
        // === 场景 B: 消息发送前 (Before Send) ===
        else if let EventType::BeforeSend(packet) = &ctx.event {
            let group_id = packet.group_id().filter(|id| *id != 0);
            let user_id = packet.user_id().unwrap_or(0);
            let Some(key) = channel_key(&ctx.bot.login_user.id, group_id, user_id) else {
                return Ok(Some(ctx));
            };

            let content = packet
                .message()
                .cloned()
                .unwrap_or_else(|| OwnedValue::from(Vec::<OwnedValue>::new()));
            let sig = content.as_array().and_then(|arr| signature(arr, &config));

            match sig {
                Some(sig) => {
                    observe(key, sig, content, Sender::Bot, &config, now);
                }
                None => break_chain(key, now),
            }
        }

        Ok(Some(ctx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(json: serde_json::Value) -> Vec<OwnedValue> {
        simd_json::serde::to_owned_value(json)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn text_chain(text: &str) -> Vec<OwnedValue> {
        chain(serde_json::json!([{"type": "text", "data": {"text": text}}]))
    }

    fn cfg() -> RepeaterConfig {
        RepeaterConfig {
            cooldown_seconds: 0,
            ..Default::default()
        }
    }

    fn feed_text(state: &mut ChannelState, text: &str, user: i64, config: &RepeaterConfig) -> Action {
        feed_text_at(state, text, user, config, 1_000)
    }

    fn feed_text_at(
        state: &mut ChannelState,
        text: &str,
        user: i64,
        config: &RepeaterConfig,
        now: u64,
    ) -> Action {
        let segments = text_chain(text);
        let sig = signature(&segments, config).expect("文本应当可复读");
        let content = OwnedValue::from(segments);
        feed(state, sig, content, Sender::User(user), config, now)
    }

    #[test]
    fn whitespace_folds_into_the_same_signature() {
        let config = cfg();
        let a = signature(&text_chain("复读  一下"), &config).unwrap();
        let b = signature(&text_chain(" 复读\n一下 "), &config).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn same_image_matches_across_different_urls() {
        let config = cfg();
        let a = signature(
            &chain(serde_json::json!([
                {"type": "image", "data": {"file": "abc.image", "url": "http://x/a?token=1"}}
            ])),
            &config,
        );
        let b = signature(
            &chain(serde_json::json!([
                {"type": "image", "data": {"file": "abc.image", "url": "http://x/a?token=2"}}
            ])),
            &config,
        );
        assert_eq!(a, b);
        assert!(a.is_some());
    }

    #[test]
    fn at_and_reply_messages_are_not_repeatable() {
        let config = cfg();
        assert!(
            signature(
                &chain(serde_json::json!([
                    {"type": "at", "data": {"qq": "10000"}},
                    {"type": "text", "data": {"text": "喂"}}
                ])),
                &config
            )
            .is_none()
        );
        assert!(
            signature(
                &chain(serde_json::json!([
                    {"type": "reply", "data": {"id": "1"}},
                    {"type": "text", "data": {"text": "喂"}}
                ])),
                &config
            )
            .is_none()
        );
    }

    #[test]
    fn overlong_and_blank_messages_are_skipped() {
        let config = RepeaterConfig {
            max_chars: 4,
            ..cfg()
        };
        assert!(signature(&text_chain("一二三四五"), &config).is_none());
        assert!(signature(&text_chain("   "), &config).is_none());
    }

    #[test]
    fn images_drop_out_when_media_is_disabled() {
        let config = RepeaterConfig {
            allow_media: false,
            ..cfg()
        };
        assert!(
            signature(
                &chain(serde_json::json!([{"type": "image", "data": {"file": "a.image"}}])),
                &config
            )
            .is_none()
        );
    }

    #[test]
    fn two_people_reaching_the_threshold_trigger_one_repeat() {
        let config = cfg();
        let mut state = ChannelState::default();
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "阿夜", 2, &config), Action::Repeat);
        // 跟读过一次就不再重复
        assert_eq!(feed_text(&mut state, "阿夜", 3, &config), Action::Silent);
    }

    #[test]
    fn one_person_spamming_does_not_count_by_default() {
        let config = cfg();
        let mut state = ChannelState::default();
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Silent);

        let config = RepeaterConfig {
            allow_same_user: true,
            ..cfg()
        };
        let mut state = ChannelState::default();
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Repeat);
    }

    #[test]
    fn the_bot_never_repeats_its_own_line() {
        let config = cfg();
        let mut state = ChannelState::default();
        let segments = text_chain("我说的");
        let sig = signature(&segments, &config).unwrap();
        let content = OwnedValue::from(segments);
        assert_eq!(
            feed(&mut state, sig, content, Sender::Bot, &config, 1_000),
            Action::Silent
        );
        assert_eq!(feed_text(&mut state, "我说的", 1, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "我说的", 2, &config), Action::Silent);
    }

    #[test]
    fn interrupted_chain_restarts_counting() {
        let config = cfg();
        let mut state = ChannelState::default();
        feed_text(&mut state, "阿夜", 1, &config);
        state.interrupt_chain();
        assert_eq!(feed_text(&mut state, "阿夜", 2, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Repeat);
    }

    #[test]
    fn cooldown_blocks_the_next_repeat() {
        let config = RepeaterConfig {
            cooldown_seconds: 60,
            ..Default::default()
        };
        let mut state = ChannelState::default();
        feed_text_at(&mut state, "第一句", 1, &config, 1_000);
        assert_eq!(
            feed_text_at(&mut state, "第一句", 2, &config, 1_000),
            Action::Repeat
        );

        feed_text_at(&mut state, "第二句", 1, &config, 1_030);
        assert_eq!(
            feed_text_at(&mut state, "第二句", 2, &config, 1_030),
            Action::Silent
        );

        feed_text_at(&mut state, "第三句", 1, &config, 1_100);
        assert_eq!(
            feed_text_at(&mut state, "第三句", 2, &config, 1_100),
            Action::Repeat
        );
    }

    #[test]
    fn zero_probability_never_repeats() {
        let config = RepeaterConfig {
            probability: 0.0,
            ..cfg()
        };
        let mut state = ChannelState::default();
        feed_text(&mut state, "阿夜", 1, &config);
        assert_eq!(feed_text(&mut state, "阿夜", 2, &config), Action::Silent);
        assert!(!state.repeated, "未命中概率时应保留再次触发的机会");
    }

    #[test]
    fn full_interrupt_probability_sends_an_interruption() {
        let config = RepeaterConfig {
            interrupt_probability: 1.0,
            interrupt_texts: vec!["打断复读".to_string()],
            ..cfg()
        };
        let mut state = ChannelState::default();
        feed_text(&mut state, "阿夜", 1, &config);
        assert_eq!(
            feed_text(&mut state, "阿夜", 2, &config),
            Action::Interrupt("打断复读".to_string())
        );
    }

    #[test]
    fn blank_interrupt_pool_falls_back_to_repeating() {
        let config = RepeaterConfig {
            interrupt_probability: 1.0,
            interrupt_texts: vec!["  ".to_string()],
            ..cfg()
        };
        let mut state = ChannelState::default();
        feed_text(&mut state, "阿夜", 1, &config);
        assert_eq!(feed_text(&mut state, "阿夜", 2, &config), Action::Repeat);
    }

    #[test]
    fn threshold_is_clamped_to_two() {
        let config = RepeaterConfig {
            min_times: 0,
            ..cfg()
        };
        let mut state = ChannelState::default();
        assert_eq!(feed_text(&mut state, "阿夜", 1, &config), Action::Silent);
        assert_eq!(feed_text(&mut state, "阿夜", 2, &config), Action::Repeat);
    }

    #[test]
    fn channel_keys_never_collide() {
        assert_ne!(
            channel_key("10000", Some(123), 0),
            channel_key("10000", None, 123)
        );
        assert_ne!(
            channel_key("10000", Some(123), 0),
            channel_key("20000", Some(123), 0)
        );
        assert_eq!(channel_key("10000", None, 0), None);
    }

    #[test]
    fn blacklist_wins_over_whitelist() {
        let config = ChannelConfig {
            white: vec![1, 2],
            black: vec![2],
        };
        assert!(allow_channel(Some(1), &config));
        assert!(!allow_channel(Some(2), &config));
        assert!(!allow_channel(Some(3), &config));
        assert!(allow_channel(None, &config), "私聊不受群名单约束");
    }

    #[test]
    fn commands_are_detected_by_prefix() {
        let prefixes = vec!["/".to_string(), "#".to_string()];
        assert!(is_command(&prefixes, " /help"));
        assert!(is_command(&prefixes, "#帮助"));
        assert!(!is_command(&prefixes, "help"));
        assert!(!is_command(&[], "/help"));
    }

    #[test]
    fn eviction_drops_idle_channels_first() {
        let now = 1_000_000;
        let mut map = HashMap::new();
        for i in 0..(MAX_CHANNELS + 16) {
            // 前 32 个是一小时前就没人说话的频道，其余都在活跃期内
            let last_active = if i < 32 {
                now - IDLE_TIMEOUT_SECS - 1
            } else {
                now - (i % 60) as u64
            };
            map.insert(
                format!("chan-{i}"),
                ChannelState {
                    last_active,
                    ..Default::default()
                },
            );
        }
        maybe_evict(&mut map, now);
        assert!(map.len() <= MAX_CHANNELS);
        assert!(!map.contains_key("chan-0"), "空闲频道应被优先清掉");
        assert!(map.contains_key("chan-32"), "活跃频道不该被误伤");
    }

    #[test]
    fn eviction_caps_the_channel_table_even_when_all_are_active() {
        let now = 1_000_000;
        let mut map = HashMap::new();
        for i in 0..(MAX_CHANNELS + 16) {
            map.insert(
                format!("chan-{i}"),
                ChannelState {
                    last_active: now - (i % 60) as u64,
                    ..Default::default()
                },
            );
        }
        maybe_evict(&mut map, now);
        assert_eq!(map.len(), MAX_CHANNELS, "全员活跃时也要收敛到上限");
    }
}
