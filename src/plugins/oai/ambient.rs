//! 群聊搭话：让 pi agent 以固定人格作为群成员之一存在，绝大多数时候沉默。
//!
//! 三段式，每一段都可以单独调参、单独复盘：
//!
//! 1. **听**——进入本插件而没被指令消费的群消息都落进内存里的滚动窗口
//!    （[`window`]），窗口就是模型能看到的全部上下文。
//! 2. **判**——群里安静下来之后，用一个便宜的多模态模型读窗口，只回一个
//!    开口意愿分（[`gate`]）。分数不过线就什么都不发生，这是常态。
//! 3. **说**——过线才唤起本机 pi（[`speak`]），带人设、带工具、带描述 Satori
//!    消息元素的 skill；产出按行拆成几条消息，再按打字速度错开发出（[`pace`]）。
//!
//! 判定与措辞分开，是因为它们的成本和失败方式都不一样：判定要便宜、要多、
//! 要能看图；措辞要慢、要少、要有工具。合成一次调用就只能两头将就。

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use serde::{Deserialize, Serialize};
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod gate;
mod pace;
mod speak;
mod vision;
mod window;

use window::Turn;

const LOG_TARGET: &str = "Plugin/OAI";

/// 内置人设。首次启动写进数据目录，之后以磁盘上那份为准——人设是要被反复
/// 打磨的东西，改一句话不该等一次编译。
const PERSONA: &str = include_str!("../../../res/ambient/persona.md");
/// 描述 Satori 消息元素的 skill；随代码走，每次启动覆盖。
const SKILL: &str = include_str!("../../../res/ambient/skills/satori-reply/SKILL.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AmbientConfig {
    /// 总开关。
    pub enabled: bool,
    /// 开启搭话的群号；空列表等于不开启。
    pub groups: Vec<i64>,
    /// 判定模型：便宜、快、能看图。
    pub gate_model: String,
    /// 发言模型，交给 pi 的 `provider/model`。
    pub reply_model: String,
    /// 发言模型的思考强度（off/minimal/low/medium/high）。
    pub thinking: String,
    /// 发言时开放给 pi 的工具白名单，逗号分隔。
    pub tools: String,
    /// 开口意愿分的门槛，0-100。调高更沉默。
    pub score_threshold: u8,
    /// 每沉默 10 分钟，门槛下调的分数：越久没说话越容易被日常话题勾起来。
    pub silence_relief_per_10min: u8,
    /// 沉默补偿的上限，防止久不发言之后见什么接什么。
    pub silence_relief_cap: u8,
    /// 送进模型的最近消息条数。
    pub context_turns: usize,
    /// 随上下文送进模型的最新图片张数；置 0 关闭图片判读。
    pub context_images: usize,
    /// 群里安静多少秒之后才判定，用来把一串刷屏并成一次。
    pub debounce_seconds: u64,
    /// 从第一条消息算起最多等多久就必须判定一次。
    pub max_wait_seconds: u64,
    /// 两次发言之间的最短间隔；被 @ 时不受此限。
    pub cooldown_seconds: u64,
    /// 每个群每小时的发言上限。
    pub hourly_limit: usize,
    /// 被 @ 或被引用时跳过判定直接开口。
    pub reply_on_mention: bool,
    /// 一次发言最多拆成几条消息。
    pub max_messages: usize,
    /// 打字速度（字/分钟）。调低更像在慢慢敲。
    pub typing_cpm: u32,
    /// 长句改用语音输入时的等效速度（字/分钟）。
    pub voice_cpm: u32,
    /// 看完消息到开始打字之间的思考时间（秒）；模型已经花掉的时间计入其中。
    pub think_seconds: f32,
    /// 判定的时间上限。
    pub gate_timeout_seconds: u64,
    /// 一次发言（含工具调用）的时间上限。
    pub reply_timeout_seconds: u64,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            groups: Vec::new(),
            gate_model: "gemini-3.5-flash-lite".to_string(),
            reply_model: "apilio/gemini-3.8-flash".to_string(),
            thinking: "low".to_string(),
            tools: "read,bash,web_search,fetch_content,get_search_content".to_string(),
            score_threshold: 45,
            silence_relief_per_10min: 5,
            silence_relief_cap: 20,
            context_turns: 20,
            context_images: 2,
            debounce_seconds: 5,
            max_wait_seconds: 30,
            cooldown_seconds: 90,
            hourly_limit: 12,
            reply_on_mention: true,
            max_messages: 3,
            typing_cpm: 150,
            voice_cpm: 420,
            think_seconds: 3.0,
            gate_timeout_seconds: 45,
            reply_timeout_seconds: 240,
        }
    }
}

impl AmbientConfig {
    fn debounce(&self) -> Duration {
        Duration::from_secs(self.debounce_seconds.clamp(1, 120))
    }

    fn max_wait(&self) -> Duration {
        Duration::from_secs(self.max_wait_seconds.clamp(self.debounce_seconds.max(1), 600))
    }

    fn cooldown(&self) -> Duration {
        Duration::from_secs(self.cooldown_seconds)
    }

    pub(crate) fn gate_timeout(&self) -> Duration {
        Duration::from_secs(self.gate_timeout_seconds.clamp(5, 300))
    }

    pub(crate) fn reply_timeout(&self) -> Duration {
        Duration::from_secs(self.reply_timeout_seconds.clamp(30, 1_800))
    }

    /// 沉默越久，门槛越低。
    ///
    /// 固定门槛下的人格只有两种结局：要么天天说话，要么再也不说话——因为群聊的
    /// 话题分布是稳定的，而门槛不动。让门槛随沉默时间缓慢下移，「大多数时候不说话，
    /// 偶尔接一句」才成为一种可以自己维持的节奏，而不是一句写在提示词里的愿望。
    fn effective_threshold(&self, silent_for: Option<Duration>) -> u8 {
        let minutes = silent_for.map_or(f64::INFINITY, |elapsed| elapsed.as_secs_f64() / 60.0);
        let relief = (minutes / 10.0 * f64::from(self.silence_relief_per_10min))
            .min(f64::from(self.silence_relief_cap));
        self.score_threshold.saturating_sub(relief as u8)
    }

    fn pace(&self) -> pace::Pace {
        pace::Pace {
            typing_cpm: self.typing_cpm,
            voice_cpm: self.voice_cpm,
            think_seconds: self.think_seconds,
        }
    }
}

/// 数据目录下的资源位置。
fn base_dir(oai_data: &Path) -> PathBuf {
    oai_data.join("ambient")
}

fn persona_path(oai_data: &Path) -> PathBuf {
    base_dir(oai_data).join("persona.md")
}

fn skill_dir(oai_data: &Path) -> PathBuf {
    base_dir(oai_data).join("skills/satori-reply")
}

/// 铺开人设与 skill。
///
/// 人设只在缺失时写入——它是给人改的，覆盖等于把管理员的打磨扔掉；
/// skill 描述的是本仓库实现的消息元素，属于代码的一部分，每次启动都对齐。
pub(crate) async fn init(oai_data: &Path) -> std::io::Result<()> {
    let persona = persona_path(oai_data);
    if let Some(parent) = persona.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if !persona.exists() {
        tokio::fs::write(&persona, PERSONA).await?;
    }
    let skill = skill_dir(oai_data);
    tokio::fs::create_dir_all(&skill).await?;
    tokio::fs::write(skill.join("SKILL.md"), SKILL).await?;
    Ok(())
}

/// 记下一条群消息，必要时安排一次判定。
///
/// 由 `oai` 插件在自己所有指令都没匹配上之后调用：能走到这里的就是普通聊天。
pub(crate) async fn observe(ctx: &Context, writer: &LockedWriter, mgr: &Arc<super::data::Manager>) {
    let config = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai").ambient;
    if !config.enabled || config.groups.is_empty() {
        return;
    }
    let Some(event) = ctx.as_message() else {
        return;
    };
    let Some(group) = event.group_id().filter(|id| config.groups.contains(id)) else {
        return;
    };

    let me = ctx.bot.login_user.id.parse::<i64>().unwrap_or_default();
    let turn = build_turn(&event, me);
    // 指令是说给机器人听的，不是群聊内容；记下来只会让人格模型学着复述指令。
    let is_command = crate::command::get_prefixes(ctx)
        .iter()
        .any(|prefix| !prefix.is_empty() && turn.text.starts_with(prefix.as_str()));
    let worth_waking = !turn.from_me && !is_command && !(turn.text.is_empty() && turn.images.is_empty());

    let start = window::with_group(group, |state| {
        state.push(turn);
        state.seq += 1;
        if !worth_waking || state.pending || state.speaking {
            return false;
        }
        state.pending = true;
        true
    });
    if !start {
        return;
    }

    let ctx = ctx.clone();
    let writer = writer.clone();
    let mgr = mgr.clone();
    tokio::spawn(async move {
        if let Err(error) = consider(&ctx, &writer, &mgr, group, config).await {
            warn!(target: LOG_TARGET, "群 {group} 搭话失败：{error:#}");
        }
    });
}

/// 事件 → 窗口里的一条消息。
fn build_turn(event: &MessageEvent<'_>, me: i64) -> Turn {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut mentions_me = false;

    if let Some(segments) = event.0.get_array("message") {
        for segment in segments {
            let kind = segment.get_str("type").unwrap_or_default();
            let Some(data) = segment.get("data") else {
                continue;
            };
            match kind {
                "text" => text.push_str(data.get_str("text").unwrap_or_default()),
                "at" => {
                    let target = data.get_str("qq").unwrap_or_default();
                    if target == me.to_string() {
                        mentions_me = true;
                        text.push_str("@我 ");
                    } else {
                        text.push_str(&format!("@{target} "));
                    }
                }
                "image" | "mface" => {
                    if let Some(url) = data
                        .get("url")
                        .or_else(|| data.get("file"))
                        .and_then(|value| value.as_str())
                        .filter(|url| url.starts_with("http"))
                    {
                        images.push(url.to_string());
                    }
                    text.push_str("[图片]");
                }
                "face" => text.push_str("[表情]"),
                "record" => text.push_str("[语音]"),
                "video" => text.push_str("[视频]"),
                "reply" => {}
                _ => {}
            }
        }
    }
    if text.trim().is_empty() && !images.is_empty() {
        text = "[图片]".to_string();
    }
    // 记录里一条消息占一行，换行与连续空格都压平，免得多行消息把上下文撑散。
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    Turn {
        user_id: event.user_id(),
        name: event.sender_name().to_string(),
        text,
        images,
        message_id: event.message_id(),
        mentions_me,
        from_me: event.user_id() == me && me != 0,
        at: event.0.get_i64("time").unwrap_or_else(|| chrono::Local::now().timestamp()),
    }
}

/// 正在判定/发言的标记；无论中途怎么退出都要还原，否则这个群从此闭嘴。
struct Speaking(i64);
impl Drop for Speaking {
    fn drop(&mut self) {
        window::with_group(self.0, |state| state.speaking = false);
    }
}

/// 等群安静下来，判定，然后决定说不说话。
async fn consider(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<super::data::Manager>,
    group: i64,
    config: AmbientConfig,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + config.max_wait();
    loop {
        let before = window::with_group(group, |state| state.seq);
        tokio::time::sleep(config.debounce()).await;
        let after = window::with_group(group, |state| state.seq);
        if before == after || Instant::now() >= deadline {
            break;
        }
    }

    let Some((turns, mentioned, silent_for)) = window::with_group(group, |state| {
        state.pending = false;
        if state.speaking {
            return None;
        }
        // 自己是最后一个说话的人：没人接话就不该再自说自话。
        if state.last_is_mine() {
            return None;
        }
        let mentioned = config.reply_on_mention && state.mentioned_since_my_last();
        if state.spoken_last_hour() >= config.hourly_limit {
            return None;
        }
        if !mentioned
            && state
                .last_spoke
                .is_some_and(|last| last.elapsed() < config.cooldown())
        {
            return None;
        }
        state.speaking = true;
        let silent_for = state.last_spoke.map(|last| last.elapsed());
        Some((state.recent(config.context_turns), mentioned, silent_for))
    }) else {
        return Ok(());
    };
    let _speaking = Speaking(group);

    if turns.is_empty() {
        return Ok(());
    }
    if !mentioned {
        let (api_base, api_key) = {
            let config = mgr.config.read().await;
            (config.api_base.clone(), config.api_key.clone())
        };
        if api_base.is_empty() || api_key.is_empty() {
            anyhow::bail!("判定模型需要 API 配置，请先设置 oai 的接口地址与密钥");
        }
        let threshold = config.effective_threshold(silent_for);
        let verdict = gate::judge(&api_base, &api_key, &config, &turns).await?;
        if verdict.score < threshold {
            debug!(
                target: LOG_TARGET,
                "群 {group} 保持沉默（{}/{}，{}）",
                verdict.score, threshold, verdict.reason
            );
            return Ok(());
        }
        info!(
            target: LOG_TARGET,
            "群 {group} 决定开口（{}/{}，{}）",
            verdict.score, threshold, verdict.reason
        );
    } else {
        info!(target: LOG_TARGET, "群 {group} 被点名，直接开口");
    }

    speak_up(ctx, writer, mgr, group, &config, &turns, mentioned).await
}

/// 让人格模型写，然后按人的节奏发出去。
#[allow(clippy::too_many_arguments)]
async fn speak_up(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<super::data::Manager>,
    group: i64,
    config: &AmbientConfig,
    turns: &[Turn],
    mentioned: bool,
) -> anyhow::Result<()> {
    let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    let data_dir = mgr.path.parent().unwrap_or(&mgr.path).to_path_buf();
    let base = base_dir(&data_dir);
    let persona = tokio::fs::read_to_string(persona_path(&data_dir))
        .await
        .unwrap_or_else(|_| PERSONA.to_string());
    // 与判定看到的是同一批图：已转码成模型收得下的格式，GIF 表情包也不例外。
    let images = vision::usable_images(turns, config.context_images).await;

    let started = Instant::now();
    let raw = speak::compose(
        &oai.pi_command,
        &base,
        &skill_dir(&data_dir),
        &persona,
        config,
        oai.pi_stall(),
        turns,
        &images,
        mentioned,
    )
    .await?;

    let utterances = match pace::parse(&raw, config.max_messages.clamp(1, 5)) {
        pace::Speech::Silent => {
            info!(target: LOG_TARGET, "群 {group} 想了想，还是没说话");
            return Ok(());
        }
        pace::Speech::Say(items) => items,
    };

    let reply_to = turns.iter().rev().find(|turn| !turn.from_me).map(|turn| turn.message_id);
    let pace = config.pace();
    tokio::time::sleep(pace.think_delay(started.elapsed())).await;

    let me = ctx.bot.login_user.id.parse::<i64>().unwrap_or_default();
    for (index, utterance) in utterances.into_iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(pace.gap()).await;
        }
        if utterance.wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f32(utterance.wait)).await;
        }
        tokio::time::sleep(pace.typing_delay(utterance.chars)).await;

        let mut message = Message::new();
        if utterance.reply && let Some(id) = reply_to {
            message = message.reply(id);
        }
        message.0.extend(utterance.message.0.iter().cloned());
        let spoken = plain_text(&utterance.message);
        if let Err(error) = send_msg(ctx, writer.clone(), Some(group), None, message).await {
            warn!(target: LOG_TARGET, "群 {group} 发言发送失败：{error}");
            break;
        }
        // 出站日志里所有插件的消息长得一样，复读机复读一句群友原话与搭话开口无从分辨。
        // 记下自己说了什么，这一行既是回放，也是唯一能确认「它真的开口了」的凭据。
        info!(target: LOG_TARGET, "群 {group} 说：{spoken}");
        window::with_group(group, |state| {
            state.push(Turn {
                user_id: me,
                name: "我".to_string(),
                text: spoken,
                images: Vec::new(),
                message_id: 0,
                mentions_me: false,
                from_me: true,
                at: chrono::Local::now().timestamp(),
            });
        });
    }
    window::with_group(group, |state| state.mark_spoke());
    Ok(())
}

/// 消息链 → 记进窗口的文字形态。
fn plain_text(message: &Message) -> String {
    let mut out = String::new();
    for segment in &message.0 {
        match segment.type_.as_str() {
            "text" => out.push_str(segment.data.get("text").and_then(|v| v.as_str()).unwrap_or("")),
            "at" => out.push_str(&format!(
                "@{} ",
                segment.data.get("qq").and_then(|v| v.as_str()).unwrap_or("")
            )),
            "face" => out.push_str("[表情]"),
            "poke" => out.push_str("[戳一戳]"),
            "dice" => out.push_str("[骰子]"),
            "rps" => out.push_str("[猜拳]"),
            "image" => out.push_str("[图片]"),
            _ => {}
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use simd_json::OwnedValue;

    fn event(value: serde_json::Value) -> OwnedValue {
        simd_json::serde::to_owned_value(value).unwrap()
    }

    #[test]
    fn defaults_stay_silent_until_a_group_is_named() {
        let config = AmbientConfig::default();
        assert!(!config.enabled);
        assert!(config.groups.is_empty());
        assert!(config.max_wait() >= config.debounce());
    }

    #[test]
    fn the_longer_it_stays_quiet_the_lower_the_bar() {
        let config = AmbientConfig::default();
        assert_eq!(
            config.effective_threshold(Some(Duration::ZERO)),
            config.score_threshold
        );
        assert_eq!(
            config.effective_threshold(Some(Duration::from_secs(20 * 60))),
            config.score_threshold - 10
        );
        // 补偿有上限，久不出声也不会见什么接什么。
        assert_eq!(
            config.effective_threshold(Some(Duration::from_secs(10 * 3_600))),
            config.score_threshold - config.silence_relief_cap
        );
        // 从没说过话等同于沉默了很久。
        assert_eq!(
            config.effective_threshold(None),
            config.score_threshold - config.silence_relief_cap
        );
    }

    #[test]
    fn legacy_and_partial_tables_fall_back_to_defaults() {
        let config: AmbientConfig = toml::from_str("groups = [123]\nunknown_key = 1").unwrap();
        assert_eq!(config.groups, [123]);
        assert_eq!(config.gate_model, AmbientConfig::default().gate_model);
    }

    #[test]
    fn turns_flatten_segments_and_notice_mentions() {
        let raw = event(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 1,
            "user_id": 42,
            "message_id": 7,
            "time": 1_788_800_000_i64,
            "sender": {"nickname": "张三", "card": "老张"},
            "message": [
                {"type": "reply", "data": {"id": "6"}},
                {"type": "at", "data": {"qq": "3373167460"}},
                {"type": "text", "data": {"text": " 你怎么看"}},
                {"type": "image", "data": {"url": "https://example.com/a.png"}},
            ],
        }));
        let turn = build_turn(&MessageEvent(&raw), 3_373_167_460);
        assert_eq!(turn.name, "老张");
        assert_eq!(turn.text, "@我 你怎么看[图片]");
        assert!(turn.mentions_me);
        assert!(!turn.from_me);
        assert_eq!(turn.images, ["https://example.com/a.png"]);
        assert_eq!(turn.at, 1_788_800_000);
    }

    #[test]
    fn own_messages_are_recognized_and_media_only_turns_keep_a_label() {
        let raw = event(serde_json::json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 1,
            "user_id": 3_373_167_460_i64,
            "message": [{"type": "image", "data": {"file": "https://example.com/b.png"}}],
        }));
        let turn = build_turn(&MessageEvent(&raw), 3_373_167_460);
        assert!(turn.from_me);
        assert_eq!(turn.text, "[图片]");
    }

    #[test]
    fn spoken_messages_are_written_back_as_readable_text() {
        let message = Message::new().at(114_514).text("这步缺前提").face(178);
        assert_eq!(plain_text(&message), "@114514 这步缺前提[表情]");
    }
}
