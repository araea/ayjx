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

use crate::adapters::satori::{LockedWriter, freshness_for, send_fresh_msg_id};
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
mod breath;
mod bridge;
mod gate;
#[cfg(test)]
#[path = "ambient/tests.rs"]
mod integration_tests;
mod memory;
mod mood;
mod pace;
mod peak;
mod speak;
mod tone;
mod vision;
mod window;

use window::Turn;

const LOG_TARGET: &str = "Plugin/OAI";

/// 内置人设。首次启动写进数据目录，之后以磁盘上那份为准——人设是要被反复
/// 打磨的东西，改一句话不该等一次编译。
const PERSONA: &str = include_str!("../../../res/ambient/persona.md");
/// 随代码走的 skill：每次启动按目录名覆盖写入。
///
/// 分成两份是照 pi 的渐进披露来的——常在提示词里的只有 skill 的一行描述，
/// 正文要模型自己去 `read`。所以「怎么在群里动手」和「怎么翻旧账」拆开各自成篇，
/// 用得上哪篇才读哪篇，常驻开销仍然只是两行描述。
const SKILLS: [(&str, &str); 2] = [
    (
        "satori-reply",
        include_str!("../../../res/ambient/skills/satori-reply/SKILL.md"),
    ),
    (
        "satori-lookup",
        include_str!("../../../res/ambient/skills/satori-lookup/SKILL.md"),
    ),
];
/// 判定用的「兴趣画像」。
///
/// 判定的唯一任务是在每条消息到来时判断「这个人格会不会想接这句话」。
/// 它只需要知道人格对什么感兴趣、规避什么、怎么接话，而完整写作人设
/// （语感、句式、节奏示例）是给发言模型用的。9KB 人设在每次判定输入里
/// 几乎是常量，却占了判定输入的一大半 token——换成这份几百字的画像，
/// 能让每轮判定便宜一大截，且不影响它判断该不该开口。
const GATE_PERSONA: &str = "\
你是 QQ 群里一个常年潜水的熟面孔。你对游戏机制、效率工具、人性、自由和命运感兴趣，
喜欢雨天、旧书和结构漂亮的论证；一个词能把你带到旧书、某个游戏机制或一句名言上，
然后你歪着用它。日常爱接梗、把别人顺口的前提拎出来、短暂装傻、把一件小事推演到荒谬处。
玩笑是为了好玩。捧不动也激不动，能说服你的只有证据，被说中会认，还会乐一下。
别人认真求助时你会认真查证并给可核实的来源，有一说一。
兴趣淡下去的地方：复读刷屏、事情已经解决、别人明确不想继续、以及表白依恋色情这类
情感纠缠——那些你会本能地岔开或者干脆看着。沉默对你是常态，不是憋着。
判断「你会不会想接这句话」即可，措辞风格不用你操心。";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AmbientConfig {
    /// 总开关。
    pub enabled: bool,
    /// 开启搭话的群号；空列表等于不开启。
    pub groups: Vec<i64>,
    /// 判定模型：便宜、快、能看图。写 `供应商/模型` 时按 `[oai.providers]` 取接口，
    /// 默认走 DeepSeek 官方接口。
    pub gate_model: String,
    /// 判定用的浓缩人设画像（见 [`GATE_PERSONA`]）。判定只需知道对什么感兴趣、
    /// 避开什么、怎么接话，不需要完整写作人设；留空则回退用完整人设（更贵）。
    pub gate_persona: String,
    /// 发言模型，交给 pi 的 `provider/model`；默认 DeepSeek 官方 `deepseek-flash`。
    pub reply_model: String,
    /// 发言模型的思考强度（off/minimal/low/medium/high）。
    pub thinking: String,
    /// 发言时开放给 pi 的工具白名单，逗号分隔。
    ///
    /// 默认这几样各有用处：`read`/`write`/`bash` 让它能在本轮工作目录里整理材料
    /// 再当文件发出去；`web_search` / `fetch_content` / `get_search_content` 是
    /// 「不知道就去查」那条路（群友贴的链接也靠 fetch_content 才真读得到）；
    /// `source_check` 用来给有争议的说法找带原文的出处。写进来的名字必须是 pi
    /// 实际注册了的工具，否则只是被静默忽略。
    pub tools: String,
    /// 开口意愿分的门槛，0-100。调高更沉默。
    pub score_threshold: u8,
    /// 每沉默 10 分钟，门槛下调的分数：越久没说话越容易被日常话题勾起来。
    pub silence_relief_per_10min: u8,
    /// 沉默补偿的上限，防止久不发言之后见什么接什么。
    pub silence_relief_cap: u8,
    /// 最近十分钟里每说过一轮，门槛上调的分数：刚接了几句的人本来就该消停一会儿。
    pub speech_penalty_per_turn: u8,
    /// 上面那笔加价的上限，免得说过几轮之后彻底哑掉。
    pub speech_penalty_cap: u8,
    /// 正在关注的话题被接住时，门槛下调的分数。取代从前的「直接放行」。
    pub focus_relief: u8,
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
    /// 记住群里的人和旧事（落盘，跨重启）。关掉就只剩眼前这几十条消息。
    pub memory_enabled: bool,
    /// 按作息与互动起伏的内部状态：影响开口门槛、打字快慢和提示词里的一句状态。
    pub mood_enabled: bool,
    /// 每轮最多写几条记忆；0 关闭 `satori_memo`。
    pub memo_budget: usize,
    /// 每轮最多查几次群聊旧账（`satori_history` / `satori_group`）；0 关闭这两个工具。
    ///
    /// 内存窗口只有几十条、且重启就空，但 QQ 自己存着完整历史与整份成员名册。
    /// 开着它，人格才能想起「上周那个报错」和「这人上次是什么时候冒头的」。
    pub lookup_budget: usize,
    /// 计价高峰时段的作息（见 [`peak`]）。DeepSeek 官方接口空闲时段半价，
    /// 而搭话是这里唯一无人触发的付费功能，最值得挑时段。
    pub peak: peak::PeakConfig,
    /// 消息时效窗口（秒）：请求交给 satori-qq 之后，群里只要又有人说话就不再发
    /// 出这一句。0 关闭。见 [`crate::adapters::satori::Freshness`]。
    pub send_freshness_seconds: u64,
    /// 一次发言最多拆成几条消息。
    pub max_messages: usize,
    /// 一条消息大约多少字就该换气：超过大约一条半的长度时，把一段话在最自然的
    /// 断句处拆成几条依次发出（总数仍受 `max_messages` 约束）；0 关闭自动分段。
    ///
    /// 模型写出来的是一整段，群友写出来的是三条——差别只在换气。见 [`breath`]。
    pub split_chars: usize,
    /// 每轮平台写动作总数（含消息、点赞、撤回）。
    pub max_actions: usize,
    /// 每轮最多生成图片的张数；0 关闭绘图。绘图走 `[oai]` 配置的图像模型。
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
            gate_model: "deepseek/deepseek-flash".to_string(),
            gate_persona: GATE_PERSONA.to_string(),
            reply_model: "deepseek/deepseek-flash".to_string(),
            thinking: "low".to_string(),
            tools: "read,write,bash,web_search,source_check,fetch_content,get_search_content"
                .to_string(),
            score_threshold: 50,
            silence_relief_per_10min: 0,
            silence_relief_cap: 0,
            speech_penalty_per_turn: 8,
            speech_penalty_cap: 24,
            focus_relief: 15,
            context_turns: 20,
            context_images: 2,
            debounce_seconds: 3,
            max_wait_seconds: 12,
            cooldown_seconds: 0,
            focus_max_seconds: 300,
            hourly_limit: 0,
            reply_on_mention: true,
            memory_enabled: true,
            mood_enabled: true,
            memo_budget: 3,
            lookup_budget: 4,
            peak: peak::PeakConfig::default(),
            send_freshness_seconds: 25,
            max_messages: 3,
            split_chars: 22,
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

    /// 这一句还值得说多久。0 表示不带时效条件，与从前一样无条件发送。
    pub(crate) fn freshness_window(&self) -> Duration {
        Duration::from_secs(if self.send_freshness_seconds == 0 {
            0
        } else {
            self.send_freshness_seconds.clamp(3, 300)
        })
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

    /// 发言节奏。精神头好就敲得快、想得短，困了反过来。
    fn pace(&self, state: mood::Snapshot) -> pace::Pace {
        let (typing, think) = if self.mood_enabled {
            (state.typing_scale(), state.think_scale())
        } else {
            (1.0, 1.0)
        };
        pace::Pace {
            typing_cpm: ((self.typing_cpm as f32) * typing).round().max(20.0) as u32,
            voice_cpm: ((self.voice_cpm as f32) * typing).round().max(20.0) as u32,
            think_seconds: self.think_seconds * think,
        }
    }

    /// 高峰时段被点名唤醒时用的一份「省着来」的配置。
    ///
    /// 输入里最贵的是图片，其次是上下文长度；输出里最贵的是多发几条和顺手画张图。
    /// 醒过来回一句仍然算数，只是这一句用最少的钱说完。
    fn frugal(&self) -> Self {
        Self {
            context_images: 0,
            context_turns: (self.context_turns / 2).max(6),
            max_messages: self.max_messages.min(2),
            draw_budget: 0,
            lookup_budget: self.lookup_budget.min(1),
            ..self.clone()
        }
    }

    /// 这一轮实际要跨过的门槛。
    ///
    /// 三笔加减：可选的沉默补偿、当下状态的微调，以及「刚才已经说了几轮」的加价。
    /// 最后这一笔是防止刷屏的主力——门槛随自己的发言次数一路抬高，人格在热闹的
    /// 群里会自然收住，而不是靠某个固定的每小时配额一刀切。
    fn threshold(
        &self,
        silent_for: Option<Duration>,
        state: mood::Snapshot,
        recent_turns: usize,
    ) -> u8 {
        let base = i16::from(self.effective_threshold(silent_for));
        let shift = if self.mood_enabled {
            state.threshold_shift()
        } else {
            0
        };
        let crowding = (recent_turns as i16)
            .saturating_mul(i16::from(self.speech_penalty_per_turn))
            .min(i16::from(self.speech_penalty_cap));
        (base + shift + crowding).clamp(1, 100) as u8
    }
}

/// 一轮判定与发言共用的「现场」。
///
/// 全部由本地数据算出，不额外调用模型：群里此刻的语感、自己的精神头、参与节奏、
/// 以及记得的人和旧事。小模型对这种具体锚点的反应，比再加十条抽象规则好得多。
pub(crate) struct Scene {
    /// 自己的发言节奏与当前关注。
    pub rhythm: String,
    /// 本群此刻的说话方式。
    pub register: String,
    /// 精神头与兴致；关闭状态时为空。
    pub state: String,
    /// 记得的人与旧事；关闭记忆时为空。
    pub memory: String,
}

impl Scene {
    fn build(group: i64, config: &AmbientConfig, turns: &[Turn], rhythm: String) -> Self {
        Self {
            rhythm,
            register: tone::register(turns),
            state: if config.mood_enabled {
                mood::snapshot(group).describe()
            } else {
                String::new()
            },
            memory: if config.memory_enabled {
                memory::with_group(group, |memory| {
                    memory.brief(turns, chrono::Local::now().timestamp())
                })
            } else {
                String::new()
            },
        }
    }

    /// 现场 → 注入提示词的一段话。
    pub(crate) fn brief(&self) -> String {
        let mut out = format!("{}\n{}\n", now_context(), self.register);
        if !self.state.is_empty() {
            out.push_str(&self.state);
            out.push('\n');
        }
        out.push_str("当前参与状态：");
        out.push_str(&self.rhythm);
        out.push('\n');
        out.push_str(&self.memory);
        out
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

fn skills_root(oai_data: &Path) -> PathBuf {
    base_dir(oai_data).join("skills")
}

/// 交给 pi 的 `--skill` 路径清单。
fn skill_dirs(oai_data: &Path) -> Vec<PathBuf> {
    let root = skills_root(oai_data);
    SKILLS.iter().map(|(name, _)| root.join(name)).collect()
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
    for (name, body) in SKILLS {
        let dir = skills_root(oai_data).join(name);
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(dir.join("SKILL.md"), body).await?;
    }
    tokio::fs::write(
        base_dir(oai_data).join("satori-tools.ts"),
        include_str!("../../../res/ambient/satori-tools.ts"),
    )
    .await?;
    tokio::fs::create_dir_all(base_dir(oai_data).join("media")).await?;
    tokio::fs::create_dir_all(base_dir(oai_data).join("memory")).await?;
    memory::attach(&base_dir(oai_data));
    mood::attach(&base_dir(oai_data));
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
    if config.memory_enabled && !turn.from_me {
        let (id, name, at) = (turn.user_id, turn.name.clone(), turn.at);
        memory::edit(group, |memory| memory.see(id, &name, at));
    }
    if config.mood_enabled && turn.mentions_me && !turn.from_me {
        mood::nudge(|mood, now| mood.engaged(group, now));
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
    if config.mood_enabled && turn.mentions_me {
        mood::nudge(|mood, now| mood.engaged(group, now));
    }
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
        // 记性和状态每批都落盘：绝大多数批次以沉默收场，只在开口时保存等于几乎不保存。
        memory::flush(group).await;
        mood::flush().await;
        if !window::with_group(group, |state| state.finish_batch(seq)) {
            worker.armed = false;
            return Ok(());
        }
    }
}

/// 判定模型要用的接口、密钥与纯模型 id。
///
/// `gate_model` 写成 `供应商/模型` 时按 `[oai.providers]` 取该供应商的接口
/// （DeepSeek 官方即走这里）；不带前缀则沿用 `oai` 默认接口，与从前一致。
async fn gate_endpoint(
    ctx: &Context,
    mgr: &Arc<super::data::Manager>,
    gate_model: &str,
) -> anyhow::Result<(String, String, String)> {
    let (provider, model) = super::utils::split_provider(gate_model);
    let providers =
        crate::plugins::get_config_or_default::<super::OaiConfig>(ctx, "oai").providers;
    let (base, key) = {
        let config = mgr.config.read().await;
        (config.api_base.clone(), config.api_key.clone())
    };
    let Some((base, key)) = super::resolve_endpoint(&providers, &base, &key, provider.as_deref())
    else {
        anyhow::bail!(
            "未知供应商：{}（在 [oai.providers] 里配置）",
            provider.as_deref().unwrap_or_default()
        );
    };
    if base.is_empty() || key.is_empty() {
        anyhow::bail!("判定模型需要 API 配置，请先设置 oai 的接口地址与密钥");
    }
    Ok((base, key, model))
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
    // 计价高峰时段：要么彻底不出声，要么睡着——不再主动判定（判定是最频繁的那次
    // 调用），只有被点名才醒一次，并且换上最省的一份上下文。
    let stance = config.peak.stance();
    let frugal;
    let config = match stance {
        peak::Stance::Asleep => {
            debug!(target: LOG_TARGET, "群 {group} 处于计价高峰时段，本轮不出声");
            return Ok(());
        }
        peak::Stance::Dozing if !mentioned => {
            debug!(target: LOG_TARGET, "群 {group} 处于计价高峰时段，睡着，没被点名就不判定");
            return Ok(());
        }
        peak::Stance::Dozing => {
            info!(target: LOG_TARGET, "群 {group} 在计价高峰时段被点名，省着回一句");
            frugal = config.frugal();
            &frugal
        }
        peak::Stance::Awake => config,
    };
    let turns = &turns[turns.len().saturating_sub(config.context_turns.clamp(1, 80))..];

    // 上一次开口是被接住了还是掉在地上，只在这里结算一次。
    if config.mood_enabled
        && let Some(gap) = window::with_group(group, |state| state.take_feedback())
    {
        mood::nudge(|mood, now| match gap {
            ..=90 => mood.engaged(group, now),
            300.. => mood.ignored(group, now),
            _ => {}
        });
    }
    let scene = Scene::build(group, config, turns, rhythm.to_string());
    if !mentioned {
        let (api_base, api_key, gate_model) = gate_endpoint(ctx, mgr, &config.gate_model).await?;
        let recent = window::with_group(group, |state| state.spoken_within(window::RECENT_SPEECH));
        let threshold = config.threshold(silent_for, mood::snapshot(group), recent);
        let verdict =
            gate::judge(&api_base, &api_key, &gate_model, config, turns, &persona, &scene, None)
                .await?;
        if !verdict.wants_composition(
            threshold,
            config.focus_relief,
            focused,
            silent_for,
            config.cooldown(),
        ) {
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
    let scene = Scene::build(group, config, &latest, rhythm);
    speak_up(
        ctx, writer, mgr, group, config, &latest, mentioned, &persona, &scene, seq,
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
    scene: &Scene,
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
        &skill_dirs(&data_dir),
        persona,
        config,
        oai.pi_stall(),
        turns,
        &images,
        mentioned,
        scene,
        Some((ctx, writer, group, seq)),
    )
    .await?;

    let (raw, focus) = attention::extract(&raw, turns, config.focus_max_seconds);
    // 沉默不需要检查草稿；新消息留给下一批。关注仍可在本轮更新。
    let silent = matches!(
        pace::parse(&raw, config.max_messages.clamp(1, 5), config.split_chars),
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
        let (api_base, api_key, gate_model) = gate_endpoint(ctx, mgr, &config.gate_model).await?;
        let fresh = Scene::build(group, config, &latest, rhythm);
        let verdict = gate::judge(
            &api_base,
            &api_key,
            &gate_model,
            config,
            &latest,
            persona,
            &fresh,
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
    let mut utterances = match pace::parse(&raw, config.max_messages.clamp(1, 5), config.split_chars) {
        pace::Speech::Silent => {
            info!(target: LOG_TARGET, "群 {group} 想了想，还是没说话");
            return Ok(());
        }
        pace::Speech::Say(items) => items,
    };
    // 人不会把刚说过的话换个标点再说一遍；小模型在同一段上下文里被反复唤起时会。
    let history = window::with_group(group, |state| state.recent(40));
    utterances.retain(|utterance| {
        let text = plain_text(&utterance.message);
        if tone::echoes(&text, &history) {
            info!(target: LOG_TARGET, "群 {group} 咽回一句复读：{text}");
            return false;
        }
        true
    });
    if utterances.is_empty() {
        return Ok(());
    }

    let reply_to = turns
        .iter()
        .rev()
        .find(|turn| !turn.from_me)
        .map(|turn| turn.message_id);
    let pace = config.pace(mood::snapshot(group));
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
        let id = match send_fresh_msg_id(
            ctx,
            writer.clone(),
            Some(group),
            None,
            &message,
            freshness_for(group, config.freshness_window()),
        )
        .await
        {
            Ok(Some(id)) => id.parse::<i64>().unwrap_or_default(),
            Ok(None) => {
                info!(target: LOG_TARGET, "群 {group} 这句话没发出去：交给 QQ 之前群里又说了话");
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
    if sent {
        if config.mood_enabled {
            mood::nudge(|mood, now| mood.spoke(group, now));
        }
        if config.memory_enabled
            && let Some(target) = turns.iter().rev().find(|turn| !turn.from_me)
        {
            let (id, at) = (target.user_id, chrono::Local::now().timestamp());
            memory::edit(group, |memory| memory.exchange(id, at));
        }
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
        assert_eq!(config.gate_model, "deepseek/deepseek-flash");
        assert_eq!(config.reply_model, "deepseek/deepseek-flash");
        // 判定人设默认是浓缩画像，比完整人设便宜得多，且不会被空值覆盖。
        assert!(!config.gate_persona.trim().is_empty());
        let custom: AmbientConfig =
            toml::from_str("gate_model = 'custom-gate'\nreply_model = 'custom/custom-reply'")
                .unwrap();
        assert_eq!(custom.gate_model, "custom-gate");
        assert_eq!(custom.reply_model, "custom/custom-reply");
        // 显式清空 gate_persona 时判定回退用完整人设。
        let no_gate_persona: AmbientConfig = toml::from_str("gate_persona = ''").unwrap();
        assert!(no_gate_persona.gate_persona.trim().is_empty());
    }

    /// 人设是每轮都要付一次钱的东西，而它天然会长：每发现一种不满意的说法，
    /// 就想再加一句话把它堵住。这个上限不是审美洁癖，是提醒——要加一段之前，
    /// 先看看能不能删两段。真正的边界只有一条（色情与情感纠缠），协议在现场说明里，
    /// 工具怎么用在 skill 里，剩下的都该是「他是谁」。
    #[test]
    fn the_persona_stays_short_enough_to_pay_for_every_round() {
        assert!(
            PERSONA.len() < 4096,
            "内置人设 {} 字节，超出预算了：先删再加",
            PERSONA.len()
        );
        // 判定用的画像比完整人设还要便宜一大截——每条消息都要过它一遍。
        assert!(GATE_PERSONA.len() < PERSONA.len());
        // 唯一的硬边界仍然写着。
        assert!(PERSONA.contains("色情"));
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

    /// 默认就带上时效条件：话说晚了不如不说。写 0 才回到从前的无条件发送。
    #[test]
    fn utterances_expire_by_default_and_the_window_stays_sane() {
        let config = AmbientConfig::default();
        assert_eq!(config.freshness_window(), Duration::from_secs(25));
        let off = AmbientConfig {
            send_freshness_seconds: 0,
            ..AmbientConfig::default()
        };
        assert!(off.freshness_window().is_zero());
        // 极端值被夹回可用区间，不会变成「一发出去就过期」或「永远有效」。
        let silly = AmbientConfig {
            send_freshness_seconds: 1,
            ..AmbientConfig::default()
        };
        assert_eq!(silly.freshness_window(), Duration::from_secs(3));
        let huge = AmbientConfig {
            send_freshness_seconds: u64::MAX,
            ..AmbientConfig::default()
        };
        assert_eq!(huge.freshness_window(), Duration::from_secs(300));
        // 旧配置里没有这两个键也读得出来，取默认值。
        let legacy: AmbientConfig = toml::from_str("groups = [1]").unwrap();
        assert_eq!(legacy.send_freshness_seconds, config.send_freshness_seconds);
        assert_eq!(legacy.lookup_budget, config.lookup_budget);
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
    fn peak_hours_default_to_dozing_through_deepseeks_expensive_window() {
        let config = AmbientConfig::default();
        assert_eq!(config.peak.mode, peak::Mode::Sleep);
        // 醒来那一轮用最省的一份：不看图、上下文减半、少发一条、不绘图、只查一次。
        let frugal = config.frugal();
        assert_eq!(frugal.context_images, 0);
        assert!(frugal.context_turns < config.context_turns);
        assert!(frugal.max_messages <= 2);
        assert_eq!(frugal.draw_budget, 0);
        assert_eq!(frugal.lookup_budget, 1);
        // 其余设置原样带过去。
        assert_eq!(frugal.reply_model, config.reply_model);
        assert_eq!(frugal.groups, config.groups);
        // 旧配置里没有这张表也能读出来。
        let legacy: AmbientConfig = toml::from_str("groups = [1]").unwrap();
        assert_eq!(legacy.peak.windows, config.peak.windows);
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
    fn the_scene_carries_every_local_anchor_and_drops_the_ones_turned_off() {
        let _guard = memory::exclusive();
        let group = -9_100_001;
        let turns: Vec<Turn> = (0..6)
            .map(|index| Turn {
                user_id: 42,
                name: "老张".into(),
                text: "这破依赖装了半天".into(),
                images: vec![],
                elements: Message::new(),
                message_id: index + 1,
                mentions_me: false,
                from_me: false,
                at: chrono::Local::now().timestamp() + index * 20,
            })
            .collect();
        memory::edit(group, |memory| {
            for _ in 0..10 {
                memory.see(42, "老张", chrono::Local::now().timestamp() - 86_400);
            }
            memory.remember(42, "在修驾校那台破电脑").unwrap();
        });
        let config = AmbientConfig::default();
        let brief = Scene::build(group, &config, &turns, "尚未发言".into()).brief();
        assert!(brief.starts_with("现在："), "{brief}");
        assert!(brief.contains("本群此刻："), "{brief}");
        assert!(brief.contains("你现在的状态："), "{brief}");
        assert!(brief.contains("当前参与状态：尚未发言"), "{brief}");
        assert!(brief.contains("在修驾校那台破电脑"), "{brief}");

        // 两个开关各自关掉自己那段，别的照旧。
        let quiet = AmbientConfig {
            memory_enabled: false,
            mood_enabled: false,
            ..AmbientConfig::default()
        };
        let brief = Scene::build(group, &quiet, &turns, "尚未发言".into()).brief();
        assert!(brief.contains("本群此刻："), "{brief}");
        assert!(!brief.contains("你现在的状态："), "{brief}");
        assert!(!brief.contains("在修驾校那台破电脑"), "{brief}");
    }

    #[test]
    fn state_moves_the_bar_and_the_keyboard_only_while_it_is_enabled() {
        let config = AmbientConfig::default();
        let tired = mood::Snapshot {
            energy: 0.15,
            warmth: 0.1,
        };
        let lively = mood::Snapshot {
            energy: 0.9,
            warmth: 0.9,
        };
        assert!(config.threshold(None, tired, 0) > config.threshold(None, lively, 0));
        assert!(config.pace(tired).typing_cpm < config.pace(lively).typing_cpm);
        assert!(config.pace(tired).think_seconds > config.pace(lively).think_seconds);
        // 门槛仍留在有效区间里，不会被状态推到 0 或爆表。
        assert!((1..=100).contains(&config.threshold(None, tired, 0)));
        let fixed = AmbientConfig {
            mood_enabled: false,
            ..AmbientConfig::default()
        };
        assert_eq!(fixed.threshold(None, tired, 0), fixed.threshold(None, lively, 0));
        assert_eq!(fixed.pace(tired).typing_cpm, fixed.typing_cpm);
    }

    #[test]
    fn the_more_it_just_said_the_higher_the_bar_gets() {
        let config = AmbientConfig::default();
        let calm = mood::Snapshot {
            energy: 0.55,
            warmth: 0.35,
        };
        let quiet = config.threshold(None, calm, 0);
        assert_eq!(quiet, config.score_threshold);
        // 说过的每一轮都在抬价，但抬到封顶就不再往上。
        assert_eq!(
            config.threshold(None, calm, 1),
            quiet + config.speech_penalty_per_turn
        );
        assert_eq!(
            config.threshold(None, calm, 9),
            quiet + config.speech_penalty_cap
        );
        // 关掉这笔加价就回到从前的行为。
        let loose = AmbientConfig {
            speech_penalty_per_turn: 0,
            ..AmbientConfig::default()
        };
        assert_eq!(loose.threshold(None, calm, 5), quiet);
    }

}
