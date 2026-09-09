//! 群聊搭话：让 pi agent 以固定人格作为群成员之一存在，绝大多数时候沉默。
//!
//! 三段式，每一段都可以单独调参、单独复盘：
//!
//! 1. **听**——进入本插件而没被指令消费的群消息都落进内存里的滚动窗口
//!    （[`window`]），窗口就是模型能看到的全部上下文。
//! 2. **判**——群里安静下来之后，用一个便宜的多模态模型读窗口，只回一个
//!    开口意愿分（[`gate`]）。分数不过线就什么都不发生，这是常态。
//! 3. **说**——过线才唤起本机 pi（[`speak`]），带人设、带工具、带描述 Satori
//!    消息元素的 skill；通过带回执的平台工具执行动作（[`bridge`]），保留旧文字输出兼容。
//!
//! 判定与措辞分开，是因为它们的成本和失败方式都不一样：判定要便宜、要多、
//! 要能看图；措辞要慢、要少、要有工具。合成一次调用就只能两头将就。

use crate::adapters::satori::{LockedWriter, send_msg_id};
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use chrono::Datelike as _;
use serde::{Deserialize, Serialize};
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod actions;
mod attention;
mod bridge;
mod gate;
#[cfg(test)]
#[path = "ambient/tests.rs"]
mod integration_tests;
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
    /// 可选硬冷却；0 关闭，被点名或正在继续感兴趣的对话时不受限。
    pub cooldown_seconds: u64,
    /// 人格一次最多关注多少秒；0 关闭，最多 600 秒，可随互动续期。
    pub focus_max_seconds: u64,
    /// 每群每小时的可选发言上限；0 关闭。
    pub hourly_limit: usize,
    /// 被 @ 或被引用时跳过判定直接开口。
    pub reply_on_mention: bool,
    /// 一次发言最多拆成几条消息。
    pub max_messages: usize,
    /// 每轮平台写动作总数（含消息、点赞、撤回）。
    pub max_actions: usize,
    /// 每轮最多生成图片的张数；0 关闭绘图。绘图走 oai 的 GPT Image 2.5 图像接口。
    pub draw_budget: usize,
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
            gate_model: "gemini-3.8-flash".to_string(),
            reply_model: "apilio/gemini-3.8-flash".to_string(),
            thinking: "low".to_string(),
            tools: "read,bash,web_search,fetch_content,get_search_content".to_string(),
            score_threshold: 45,
            silence_relief_per_10min: 0,
            silence_relief_cap: 0,
            context_turns: 20,
            context_images: 2,
            debounce_seconds: 3,
            max_wait_seconds: 12,
            cooldown_seconds: 0,
            focus_max_seconds: 300,
            hourly_limit: 0,
            reply_on_mention: true,
            max_messages: 3,
            max_actions: 6,
            draw_budget: 2,
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
        Duration::from_secs(self.max_wait_seconds.clamp(self.debounce().as_secs(), 600))
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

    /// 可选的旧版沉默补偿；默认关闭，不为刷存在感降低人格的兴趣门槛。
    fn effective_threshold(&self, silent_for: Option<Duration>) -> u8 {
        if self.silence_relief_per_10min == 0 {
            return self.score_threshold;
        }
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

/// 当前本机日期、星期与时刻，让判定与发言模型感知「现在几点」。
///
/// 群聊语境里早晚、工作日与周末是有信息量的——模型拿不到真实时钟，只能靠文本。
pub(crate) fn now_context() -> String {
    let now = chrono::Local::now();
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    };
    format!(
        "现在：{} {} {}（本机时间）",
        now.format("%Y-%m-%d"),
        weekday,
        now.format("%H:%M")
    )
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
    tokio::fs::write(
        base_dir(oai_data).join("satori-tools.ts"),
        include_str!("../../../res/ambient/satori-tools.ts"),
    )
    .await?;
    tokio::fs::create_dir_all(base_dir(oai_data).join("media")).await?;
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
        observe_notice(ctx, writer, mgr, &config).await;
        return;
    };
    let Some(group) = event.group_id().filter(|id| config.groups.contains(id)) else {
        return;
    };

    let me = ctx.bot.login_user.id.parse::<i64>().unwrap_or_default();
    let mut turn = build_turn(&event, me);
    turn.mentions_me |= window::with_group(group, |state| {
        event.0.get_array("message").is_some_and(|segments| {
            segments.iter().any(|segment| {
                segment.get_str("type") == Some("reply")
                    && segment
                        .get("data")
                        .and_then(|data| {
                            data.get_str("id")
                                .and_then(|id| id.parse::<i64>().ok())
                                .or_else(|| data.get_i64("id"))
                        })
                        .is_some_and(|id| state.is_own_message(id))
            })
        })
    });
    // 指令是说给机器人听的，不是群聊内容；记下来只会让人格模型学着复述指令。
    let is_command = crate::command::get_prefixes(ctx)
        .iter()
        .any(|prefix| !prefix.is_empty() && turn.text.starts_with(prefix.as_str()));
    if is_command || (turn.text.is_empty() && turn.images.is_empty()) {
        return;
    }
    let start = window::with_group(group, |state| state.receive(turn));
    if !start {
        return;
    }

    let ctx = ctx.clone();
    let writer = writer.clone();
    let mgr = mgr.clone();
    tokio::spawn(async move {
        if let Err(error) = consider(&ctx, &writer, &mgr, group).await {
            warn!(target: LOG_TARGET, "群 {group} 搭话失败：{error:#}");
        }
    });
}

/// 戳一戳和撤回也是互动；表态没有操作者，不能凭空归到某位群友头上。
async fn observe_notice(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<super::data::Manager>,
    config: &AmbientConfig,
) {
    let crate::event::EventType::Satori(raw) = &ctx.event else {
        return;
    };
    let Some(group) = raw
        .get_i64("group_id")
        .filter(|g| config.groups.contains(g))
    else {
        return;
    };
    let Some((turn, recalled)) = notice_turn(raw, ctx.bot.login_user.id.parse().unwrap_or(0))
    else {
        return;
    };
    let start = window::with_group(group, |state| {
        if let Some(id) = recalled {
            state.recall(id);
        }
        state.receive(turn)
    });
    if start {
        let (ctx, writer, mgr) = (ctx.clone(), writer.clone(), mgr.clone());
        tokio::spawn(async move {
            if let Err(error) = consider(&ctx, &writer, &mgr, group).await {
                warn!(target: LOG_TARGET, "群 {group} 互动处理失败：{error:#}");
            }
        });
    }
}

fn notice_turn(raw: &simd_json::OwnedValue, me: i64) -> Option<(Turn, Option<i64>)> {
    let user = raw.get_i64("user_id").unwrap_or(0);
    let mid = raw.get_i64("message_id").unwrap_or(0);
    let kind = raw.get_str("satori_type").unwrap_or("");
    let mut recalled = None;
    let mut mentions_me = false;
    let mut from_me = user == me;
    let text = match kind {
        "internal" if raw.get_str("sub_type") == Some("poke") => {
            let data = raw.get("satori_data")?;
            let target = data
                .get_str("target_id")
                .and_then(|s| s.parse::<i64>().ok())
                .or_else(|| data.get_i64("target_id"))
                .unwrap_or(0);
            mentions_me = target == me && user != me;
            format!("[戳一戳：{user} 戳了 {target}]")
        }
        "message-deleted" => {
            recalled = Some(mid);
            format!("[消息 {mid} 已撤回]")
        }
        "reaction-added" | "reaction-removed" => {
            let emoji = raw
                .get("_satori")?
                .get("emoji")?
                .get_str("id")
                .unwrap_or("?");
            // 只记录、不靠这类无操作者回声唤醒，避免自己点赞→自己接话的循环。
            from_me = true;
            format!(
                "[平台事件：消息 {mid} {}表态 {emoji}；操作者未知]",
                if kind == "reaction-added" {
                    "新增"
                } else {
                    "减少"
                }
            )
        }
        _ => return None,
    };
    Some((
        Turn {
            user_id: user,
            name: if from_me && user == 0 {
                "平台事件".into()
            } else {
                user.to_string()
            },
            text,
            images: vec![],
            elements: Message::new(),
            message_id: 0,
            mentions_me,
            from_me,
            at: raw
                .get_i64("time")
                .unwrap_or_else(|| chrono::Local::now().timestamp()),
        },
        recalled,
    ))
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
                "face" => text.push_str(&format!("[表情:{}]", data.get_str("id").unwrap_or("?"))),
                "record" => text.push_str("[语音]"),
                "video" => text.push_str("[视频]"),
                "reply" => text.push_str(&format!("[引用:{}] ", data.get_str("id").unwrap_or("?"))),
                "file" => text.push_str(&format!(
                    "[文件:{}]",
                    data.get_str("name").unwrap_or("未命名")
                )),
                "forward" | "node" => text.push_str("[合并转发，可用 satori_read 展开]"),
                "poke" => text.push_str("[戳一戳]"),
                "dice" => text.push_str("[骰子]"),
                "rps" => text.push_str("[猜拳]"),
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
        elements: event
            .0
            .get("message")
            .and_then(|v| simd_json::serde::from_owned_value(v.clone()).ok())
            .unwrap_or_default(),
        message_id: event.message_id(),
        mentions_me,
        from_me: event.user_id() == me && me != 0,
        at: event
            .0
            .get_i64("time")
            .unwrap_or_else(|| chrono::Local::now().timestamp()),
    }
}

/// 取消/异常时释放 worker；正常交接已在锁内完成，不能再清掉新 worker 的标记。
struct Worker {
    group: i64,
    armed: bool,
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.armed {
            window::with_group(self.group, |state| state.running = false);
        }
    }
}

async fn consider(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<super::data::Manager>,
    group: i64,
) -> anyhow::Result<()> {
    let mut worker = Worker { group, armed: true };
    loop {
        let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
        let config = oai.ambient;
        if config.focus_max_seconds == 0 {
            window::with_group(group, |state| state.focus = None);
        }
        if !oai.enabled || !config.enabled || !config.groups.contains(&group) {
            return Ok(());
        }
        let deadline = Instant::now() + config.max_wait();
        loop {
            let before = window::with_group(group, |state| state.seq);
            let delay = window::with_group(group, |state| {
                if state.active_focus().is_some() {
                    config.debounce().min(Duration::from_secs(1))
                } else {
                    config.debounce()
                }
            });
            tokio::time::sleep(delay.min(deadline.saturating_duration_since(Instant::now()))).await;
            let after = window::with_group(group, |state| state.seq);
            if before == after || Instant::now() >= deadline {
                break;
            }
        }
        let (mut seq, turns, mentioned, silent_for, rhythm, focused, capped) =
            window::with_group(group, |state| {
                let mentioned = state.take_mention() && config.reply_on_mention;
                let capped =
                    config.hourly_limit > 0 && state.spoken_last_hour() >= config.hourly_limit;
                (
                    state.seq,
                    state.recent(config.context_turns.clamp(1, 80)),
                    mentioned,
                    state.last_spoke.map(|last| last.elapsed()),
                    state.rhythm(),
                    state.active_focus().is_some(),
                    capped,
                )
            });
        if !capped
            && let Err(error) = consider_batch(
                ctx, writer, mgr, group, &config, &mut seq, &turns, mentioned, silent_for, &rhythm,
                focused,
            )
            .await
        {
            warn!(target: LOG_TARGET, "群 {group} 搭话失败：{error:#}");
        }
        if !window::with_group(group, |state| state.finish_batch(seq)) {
            worker.armed = false;
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn consider_batch(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<super::data::Manager>,
    group: i64,
    config: &AmbientConfig,
    seq: &mut u64,
    turns: &[Turn],
    mentioned: bool,
    silent_for: Option<Duration>,
    rhythm: &str,
    focused: bool,
) -> anyhow::Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let data_dir = mgr.path.parent().unwrap_or(&mgr.path);
    let persona = tokio::fs::read_to_string(persona_path(data_dir))
        .await
        .unwrap_or_else(|_| PERSONA.to_string());
    if !mentioned {
        let (api_base, api_key) = {
            let config = mgr.config.read().await;
            (config.api_base.clone(), config.api_key.clone())
        };
        if api_base.is_empty() || api_key.is_empty() {
            anyhow::bail!("判定模型需要 API 配置，请先设置 oai 的接口地址与密钥");
        }
        let threshold = config.effective_threshold(silent_for);
        let verdict =
            gate::judge(&api_base, &api_key, config, turns, &persona, rhythm, None).await?;
        if !verdict.wants_composition(threshold, focused, silent_for, config.cooldown()) {
            debug!(target: LOG_TARGET, "群 {group} 保持沉默（{}/{}，{}）", verdict.score, threshold, verdict.reason);
            return Ok(());
        }
        info!(target: LOG_TARGET, "群 {group} 交给人格决定（{}/{}，续聊={}，{}）",
            verdict.score, threshold, verdict.continuation, verdict.reason);
    } else {
        info!(target: LOG_TARGET, "群 {group} 被点名，由人格决定是否回应");
    }
    // 判定之后重新取最新窗口，群友连续发几条消息不必从头再筛一遍。
    let (latest, mentioned, rhythm) = window::with_group(group, |state| {
        *seq = state.seq;
        (
            state.recent(config.context_turns.clamp(1, 80)),
            (state.take_mention() && config.reply_on_mention) || mentioned,
            state.rhythm(),
        )
    });
    if !current(ctx, group, *seq) {
        return Ok(());
    }
    speak_up(
        ctx, writer, mgr, group, config, &latest, mentioned, &persona, &rhythm, seq,
    )
    .await
}

/// 停用配置或群聊推进后，放弃尚未发送的内容，交回 worker 读取新上下文。
fn current(ctx: &Context, group: i64, seq: u64) -> bool {
    let config = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    config.enabled
        && config.ambient.enabled
        && config.ambient.groups.contains(&group)
        && window::with_group(group, |state| state.seq == seq)
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
    persona: &str,
    rhythm: &str,
    seq: &mut u64,
) -> anyhow::Result<()> {
    let oai = crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai");
    let data_dir = mgr.path.parent().unwrap_or(&mgr.path).to_path_buf();
    let base = base_dir(&data_dir);
    // 与判定看到的是同一批图：已转码成模型收得下的格式，GIF 表情包也不例外。
    let images = vision::usable_images(turns, config.context_images).await;

    let started = Instant::now();
    let raw = speak::compose(
        &oai.pi_command,
        &base,
        &skill_dir(&data_dir),
        persona,
        config,
        oai.pi_stall(),
        turns,
        &images,
        mentioned,
        rhythm,
        Some((ctx, writer, group, seq)),
    )
    .await?;

    let (raw, focus) = attention::extract(&raw, turns, config.focus_max_seconds);
    // 沉默不需要检查草稿；新消息留给下一批。关注仍可在本轮更新。
    let silent = matches!(
        pace::parse(&raw, config.max_messages.clamp(1, 5)),
        pace::Speech::Silent
    );
    if !silent && !current(ctx, group, *seq) {
        let (latest_seq, latest, rhythm) = window::with_group(group, |state| {
            (
                state.seq,
                state.recent(config.context_turns.clamp(1, 80)),
                state.rhythm(),
            )
        });
        if !current(ctx, group, latest_seq) {
            return Ok(());
        }
        let (api_base, api_key) = {
            let config = mgr.config.read().await;
            (config.api_base.clone(), config.api_key.clone())
        };
        let verdict = gate::judge(
            &api_base,
            &api_key,
            config,
            &latest,
            persona,
            &rhythm,
            Some(&raw),
        )
        .await?;
        if verdict.score < 50 || !current(ctx, group, latest_seq) {
            debug!(target: LOG_TARGET, "群 {group} 收起过时草稿：{}", verdict.reason);
            return Ok(());
        }
        // 检查通过的这批也已处理；不能发完再把同一批当作新消息回应。
        window::with_group(group, |state| {
            if state.seq == latest_seq {
                state.take_mention();
            }
        });
        *seq = latest_seq;
    }
    if let Some(focus) = focus {
        window::with_group(group, |state| state.focus = focus);
    }
    let utterances = match pace::parse(&raw, config.max_messages.clamp(1, 5)) {
        pace::Speech::Silent => {
            info!(target: LOG_TARGET, "群 {group} 想了想，还是没说话");
            return Ok(());
        }
        pace::Speech::Say(items) => items,
    };

    let reply_to = turns
        .iter()
        .rev()
        .find(|turn| !turn.from_me)
        .map(|turn| turn.message_id);
    let pace = config.pace();
    tokio::time::sleep(pace.think_delay(started.elapsed())).await;

    let me = ctx.bot.login_user.id.parse::<i64>().unwrap_or_default();
    let mut sent = false;
    for (index, utterance) in utterances.into_iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(pace.gap()).await;
        }
        if utterance.wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f32(utterance.wait)).await;
        }
        let typing = pace.typing_delay(utterance.chars);
        // 模型耗时已经是等待；首条不再额外假装打字十几秒。
        tokio::time::sleep(if index == 0 {
            typing.saturating_sub(started.elapsed())
        } else {
            typing
        })
        .await;
        if !current(ctx, group, *seq) {
            break;
        }

        let mut message = Message::new();
        if utterance.reply
            && let Some(id) = reply_to
        {
            message = message.reply(id);
        }
        message.0.extend(utterance.message.0.iter().cloned());
        let spoken = plain_text(&utterance.message);
        let id = match send_msg_id(ctx, writer.clone(), Some(group), None, &message).await {
            Ok(Some(id)) => id.parse::<i64>().unwrap_or_default(),
            Ok(None) => {
                warn!(target: LOG_TARGET, "群 {group} 无消息回执，不计入成功发言");
                break;
            }
            Err(error) => {
                warn!(target: LOG_TARGET, "群 {group} 发言发送失败：{error}");
                break;
            }
        };
        // 出站日志里所有插件的消息长得一样，复读机复读一句群友原话与搭话开口无从分辨。
        // 记下自己说了什么，这一行既是回放，也是唯一能确认「它真的开口了」的凭据。
        info!(target: LOG_TARGET, "群 {group} 说：{spoken}");
        window::with_group(group, |state| {
            if !sent {
                state.mark_spoke();
            }
            // 服务端自发事件可能先到；按回执 ID 去重。
            state.receive(Turn {
                user_id: me,
                name: "我".to_string(),
                text: spoken,
                elements: message.clone(),
                images: Vec::new(),
                message_id: id,
                mentions_me: false,
                from_me: true,
                at: chrono::Local::now().timestamp(),
            });
        });
        sent = true;
    }
    Ok(())
}

/// 消息链 → 记进窗口的文字形态。
fn plain_text(message: &Message) -> String {
    let mut out = String::new();
    for segment in &message.0 {
        match segment.type_.as_str() {
            "text" => out.push_str(
                segment
                    .data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            ),
            "at" => out.push_str(&format!(
                "@{} ",
                segment
                    .data
                    .get("qq")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            )),
            "face" => out.push_str("[表情]"),
            "poke" => out.push_str("[戳一戳]"),
            "dice" => out.push_str("[骰子]"),
            "rps" => out.push_str("[猜拳]"),
            "image" | "mface" => out.push_str("[图片/表情包]"),
            "file" => out.push_str("[文件]"),
            "record" => out.push_str("[语音]"),
            "video" => out.push_str("[视频]"),
            "node" | "forward" => out.push_str("[合并转发]"),
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
    fn default_models_match_but_explicit_overrides_are_preserved() {
        let config: AmbientConfig = toml::from_str("").unwrap();
        assert_eq!(config.gate_model, "gemini-3.8-flash");
        assert_eq!(config.reply_model, "apilio/gemini-3.8-flash");
        let custom: AmbientConfig =
            toml::from_str("gate_model = 'custom-gate'\nreply_model = 'custom/custom-reply'")
                .unwrap();
        assert_eq!(custom.gate_model, "custom-gate");
        assert_eq!(custom.reply_model, "custom/custom-reply");
    }

    #[test]
    fn defaults_stay_silent_until_a_group_is_named() {
        let config = AmbientConfig::default();
        assert!(!config.enabled);
        assert!(config.groups.is_empty());
        assert!(config.max_wait() >= config.debounce());
        assert_eq!(config.cooldown(), Duration::ZERO);
        assert_eq!(config.hourly_limit, 0);
        assert_eq!(config.effective_threshold(None), config.score_threshold);
        let extreme = AmbientConfig {
            debounce_seconds: u64::MAX,
            max_wait_seconds: 0,
            silence_relief_cap: 20,
            ..config
        };
        assert!(extreme.max_wait() >= extreme.debounce());
        assert_eq!(extreme.effective_threshold(None), extreme.score_threshold);
    }

    #[test]
    fn the_longer_it_stays_quiet_the_lower_the_bar() {
        let config = AmbientConfig {
            silence_relief_per_10min: 5,
            silence_relief_cap: 20,
            ..AmbientConfig::default()
        };
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
        assert_eq!(turn.text, "[引用:6] @我 你怎么看[图片]");
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
    fn platform_events_preserve_targets_without_inventing_reaction_authors() {
        let poke = event(
            serde_json::json!({"satori_type":"internal","sub_type":"poke","user_id":42,"satori_data":{"target_id":"10000"}}),
        );
        let (turn, _) = notice_turn(&poke, 10000).unwrap();
        assert!(turn.mentions_me);
        assert!(!turn.from_me);
        assert_eq!(turn.message_id, 0);
        let reaction = event(
            serde_json::json!({"satori_type":"reaction-added","message_id":123,"_satori":{"emoji":{"id":"76"}}}),
        );
        let (turn, _) = notice_turn(&reaction, 10000).unwrap();
        assert_eq!(turn.user_id, 0);
        assert!(turn.from_me);
        assert!(!turn.mentions_me);
        assert!(window::transcript(&[turn]).contains("操作者未知"));
        let recall = event(
            serde_json::json!({"satori_type":"message-deleted","message_id":123,"user_id":42}),
        );
        assert_eq!(notice_turn(&recall, 10000).unwrap().1, Some(123));
    }

    #[test]
    fn spoken_messages_are_written_back_as_readable_text() {
        let message = Message::new().at(114_514).text("这步缺前提").face(178);
        assert_eq!(plain_text(&message), "@114514 这步缺前提[表情]");
    }

    #[test]
    fn now_context_reports_the_local_clock() {
        let text = now_context();
        let year = chrono::Local::now().format("%Y").to_string();
        let hour = chrono::Local::now().format("%H:%M").to_string();
        assert!(text.starts_with("现在："), "{text}");
        assert!(text.contains(&year), "{text}");
        assert!(text.contains(&hour), "{text}");
        assert!(text.contains("（本机时间）"), "{text}");
    }
}
