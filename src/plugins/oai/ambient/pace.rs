//! 把模型的一段输出拆成「像人一样发出去」的若干条消息，并给出每条的节奏。
//!
//! 两件事在这里合并处理，因为它们其实是一件事：群里发言的自然感一半来自内容
//! 怎么断句，一半来自这些断句之间隔了多久。模型只管写，断句与等待都在这里定。

use super::breath;
use crate::message::Message;
use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;

/// 一条待发的消息。
#[derive(Debug)]
pub(crate) struct Utterance {
    pub message: Message,
    /// 正文字数，用来估算「打字」耗时；戳一戳、骰子这类没有打字过程。
    pub chars: usize,
    /// 以引用触发消息的形式发出。
    pub reply: bool,
    /// 发出前额外停顿的秒数（模型显式要求的 `[wait:n]`）。
    pub wait: f32,
}

/// 模型这一轮的决定。
#[derive(Debug)]
pub(crate) enum Speech {
    /// 闭嘴。人设允许它随时改主意不说话。
    Silent,
    Say(Vec<Utterance>),
}

/// 显式停顿的上限，防止模型用一个 `[wait:9999]` 把发言拖到天荒地老。
const MAX_WAIT_SECONDS: f32 = 30.0;

fn markup() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[(at|face|img):([^\]]{1,512})\]").unwrap())
}

/// 一行里所有标记所占的字符区间（半开），交给 [`breath`] 护住。
fn markup_spans(line: &str) -> Vec<std::ops::Range<usize>> {
    markup()
        .find_iter(line)
        .map(|found| {
            let start = line[..found.start()].chars().count();
            start..start + found.as_str().chars().count()
        })
        .collect()
}

fn action() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[(poke:(\d{5,12})|dice|rps|wait:(\d+(?:\.\d+)?))\]$").unwrap())
}

/// 解析出来但还没定形的一条：动作照原样，文字要先等断句分完剩下的额度。
enum Draft {
    Act(Message),
    Text { body: String, reply: bool },
}

/// 解析模型输出：一行一条消息，标记按 `satori-reply` skill 的约定翻译成消息段。
///
/// 行数是下限而不是上限：模型写了几行就是它自己分好的几条，没用完的消息额度
/// 交给 [`breath`] 去补——一口气写完的长句，在它自己的换气处切开再依次发出。
///
/// 不认识的方括号原样保留——群友本来就会打 `[笑]`，把它们吞掉比留着更糟。
pub(crate) fn parse(raw: &str, max_messages: usize, split_chars: usize) -> Speech {
    let mut drafts: Vec<(Draft, f32)> = Vec::new();
    let mut pending_wait = 0.0_f32;

    for line in raw.lines() {
        let line = strip_decoration(line);
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("[silent]") {
            return Speech::Silent;
        }
        if let Some(caps) = action().captures(line) {
            if let Some(seconds) = caps.get(3) {
                pending_wait = (pending_wait + seconds.as_str().parse::<f32>().unwrap_or(0.0))
                    .min(MAX_WAIT_SECONDS);
                continue;
            }
            if drafts.len() >= max_messages {
                continue;
            }
            let message = match caps.get(2) {
                Some(target) => Message::new().poke(target.as_str()),
                None if caps[1].starts_with("dice") => Message::new().dice(),
                None => Message::new().rps(),
            };
            drafts.push((Draft::Act(message), std::mem::take(&mut pending_wait)));
            continue;
        }
        if drafts.len() >= max_messages {
            continue;
        }
        let (reply, body) = match line.strip_prefix("[reply]") {
            Some(rest) => (true, rest.trim()),
            None => (false, line),
        };
        if body.is_empty() {
            continue;
        }
        drafts.push((
            Draft::Text {
                body: body.to_string(),
                reply,
            },
            std::mem::take(&mut pending_wait),
        ));
    }

    // 没用完的额度就是还能换几次气；按出现顺序分给写得最长的那几行。
    let mut spare = max_messages.saturating_sub(drafts.len());
    let mut out: Vec<Utterance> = Vec::new();
    for (draft, wait) in drafts {
        match draft {
            Draft::Act(message) => out.push(Utterance {
                message,
                chars: 0,
                reply: false,
                wait,
            }),
            Draft::Text { body, reply } => {
                // 标记本身不能切，但带标记的长句照样要换气：护住 `[at:…]`、`[img:…]`
                // 这些 token 的下标，标记之外该切还切。从前是整行不动，于是一句
                // `[at:…] + 一长段` 会原样发成一条几百字不带标点的长文。
                let shield = markup_spans(&body);
                let pieces = breath::split_protected(&body, spare + 1, split_chars, &shield);
                spare = spare.saturating_sub(pieces.len().saturating_sub(1));
                for (index, piece) in pieces.into_iter().enumerate() {
                    let (message, chars) = build_message(&piece);
                    if message.0.is_empty() {
                        continue;
                    }
                    out.push(Utterance {
                        message,
                        chars,
                        // 引用只挂在第一条上：后面几条是同一口气里接着说的。
                        reply: reply && index == 0,
                        wait: if index == 0 { wait } else { 0.0 },
                    });
                }
            }
        }
    }

    if out.is_empty() {
        Speech::Silent
    } else {
        Speech::Say(out)
    }
}

/// 去掉模型偶尔带上的代码围栏、列表符号与首尾空白。
fn strip_decoration(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with("```") {
        return "";
    }
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line);
    line.trim()
}

/// 一行文本 → 消息段 + 正文字数。
fn build_message(body: &str) -> (Message, usize) {
    let mut message = Message::new();
    let mut chars = 0usize;
    let mut cursor = 0usize;

    let push_text = |message: Message, text: &str, chars: &mut usize| {
        if text.is_empty() {
            return message;
        }
        *chars += text.chars().count();
        message.text(text)
    };

    for caps in markup().captures_iter(body) {
        let whole = caps.get(0).expect("regex match has group 0");
        message = push_text(message, &body[cursor..whole.start()], &mut chars);
        cursor = whole.end();
        let value = caps[2].trim();
        message = match &caps[1] {
            "at" if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() => {
                chars += 4;
                message.at(value).text(" ")
            }
            "face" if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() => {
                message.face(value)
            }
            "img" if value.starts_with("http://") || value.starts_with("https://") => {
                message.image(value)
            }
            // 参数不合法就当普通文字，别悄悄吞掉内容。
            _ => push_text(message, whole.as_str(), &mut chars),
        };
    }
    message = push_text(message, &body[cursor..], &mut chars);
    (message, chars)
}

/// 发言节奏。
pub(crate) struct Pace {
    /// 逐字打字速度（字/分钟）。
    pub typing_cpm: u32,
    /// 长句改用语音输入时的等效速度（字/分钟）。
    pub voice_cpm: u32,
    /// 看完消息到开始打字之间的思考时间（秒）。
    pub think_seconds: f32,
}

impl Pace {
    /// 想好之前的停顿。模型已经花掉的时间算作思考，不再重复等待。
    pub(crate) fn think_delay(&self, elapsed: Duration) -> Duration {
        let target = jitter(self.think_seconds.max(0.0), 0.45);
        seconds(target - elapsed.as_secs_f32())
    }

    /// 敲完这条消息要多久。
    pub(crate) fn typing_delay(&self, chars: usize) -> Duration {
        if chars == 0 {
            // 戳一戳、骰子：抬手就发，没有打字过程。
            return seconds(jitter(0.8, 0.4));
        }
        let cpm = self.effective_cpm(chars).max(30.0);
        seconds(jitter(chars as f32 * 60.0 / cpm, 0.3).clamp(1.2, 40.0))
    }

    /// 两条消息之间的换气。
    ///
    /// 偶尔会长出一截：手机上打着字被别的事岔开一下，是群聊里最常见的停顿，
    /// 而每条都精确地隔一秒才是机器的样子。
    pub(crate) fn gap(&self) -> Duration {
        if rand::random::<f32>() < 0.15 {
            return seconds(jitter(3.2, 0.6));
        }
        seconds(jitter(0.9, 0.5))
    }

    /// 短句一个字一个字敲；长句更像按住语音键一口气说完再转写，字均耗时更低。
    fn effective_cpm(&self, chars: usize) -> f32 {
        let typing = self.typing_cpm.max(1) as f32;
        let voice = self.voice_cpm.max(self.typing_cpm) as f32;
        let ratio = ((chars as f32 - 12.0) / 48.0).clamp(0.0, 1.0);
        typing + (voice - typing) * ratio
    }
}

/// 在 `value` 上下浮动 `spread` 比例。
fn jitter(value: f32, spread: f32) -> f32 {
    value * (1.0 - spread + rand::random::<f32>() * spread * 2.0)
}

fn seconds(value: f32) -> Duration {
    Duration::from_secs_f32(value.clamp(0.0, 120.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use simd_json::base::ValueAsScalar;

    fn text_of(item: &Utterance) -> String {
        item.message
            .0
            .iter()
            .filter_map(|segment| segment.data.get("text").and_then(|v| v.as_str()))
            .collect()
    }

    fn say(raw: &str) -> Vec<Utterance> {
        match parse(raw, 3, 0) {
            Speech::Say(items) => items,
            Speech::Silent => panic!("expected speech, got silence: {raw}"),
        }
    }

    #[test]
    fn silence_wins_over_anything_else_on_the_line() {
        assert!(matches!(parse("[silent]", 3, 0), Speech::Silent));
        assert!(matches!(parse("  \n\n  ", 3, 0), Speech::Silent));
        assert!(matches!(parse("说点什么\n[silent]", 3, 0), Speech::Silent));
    }

    #[test]
    fn each_line_becomes_a_message_and_extra_lines_are_dropped() {
        let items = say("你确定？\n- 那你重读第二段\n第三条\n第四条");
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[1].message.0[0].data.get("text").unwrap(),
            "那你重读第二段"
        );
        assert!(items.iter().all(|item| !item.reply));
    }

    #[test]
    fn markup_becomes_segments_and_reply_marks_the_quote() {
        let items = say("[reply][at:114514] 第三步缺了个前提 [face:178]");
        assert_eq!(items.len(), 1);
        assert!(items[0].reply);
        let kinds: Vec<&str> = items[0]
            .message
            .0
            .iter()
            .map(|segment| segment.type_.as_str())
            .collect();
        assert_eq!(kinds, ["at", "text", "text", "face"]);
        assert!(items[0].chars > 4);
    }

    /// 带 `[at:…]` 的长句照样要换气：护住标记，标记之外照切。
    #[test]
    fn a_long_line_with_markup_still_breathes() {
        let raw = "[at:114514] 第一步把依赖装上 第二步重跑一次 第三步贴出错的第一行 别把整个日志都发出来";
        let Speech::Say(items) = parse(raw, 3, 14) else {
            panic!("expected speech");
        };
        assert!(items.len() > 1, "{items:?}");
        // @ 留在第一条上，标记本身没被切开。
        assert_eq!(items[0].message.0[0].type_, "at");
        assert!(
            items
                .iter()
                .all(|item| text_of(item).matches('[').count() == text_of(item).matches(']').count()),
            "{items:?}"
        );
    }

    #[test]
    fn unknown_brackets_stay_as_text() {
        let items = say("[笑] [at:abc] [face:] 收到");
        let text: String = items[0]
            .message
            .0
            .iter()
            .filter_map(|segment| segment.data.get("text").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(text, "[笑] [at:abc] [face:] 收到");
    }

    #[test]
    fn actions_and_waits_attach_to_the_next_message() {
        let items = say("[wait:3]\n[poke:114514]\n[dice]");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].message.0[0].type_, "poke");
        assert_eq!(items[0].chars, 0);
        assert!((items[0].wait - 3.0).abs() < f32::EPSILON);
        assert_eq!(items[1].message.0[0].type_, "dice");
        assert_eq!(items[1].wait, 0.0);
    }

    /// 模型写一行、群里看到三条：没用完的额度拿去换气。
    #[test]
    fn one_breathless_line_spends_the_unused_message_budget() {
        let raw = "坟挖得挺熟练 一看就不是第一次爬出来所以 Pro 比 Flash 强在哪 强在它不承认自己死了";
        let Speech::Say(items) = parse(raw, 3, 22) else {
            panic!("expected speech");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(text_of(&items[1]), "强在它不承认自己死了");
        // 关掉分段就回到一行一条。
        let Speech::Say(single) = parse(raw, 3, 0) else {
            panic!("expected speech");
        };
        assert_eq!(single.len(), 1);
    }

    /// 模型自己分好的几行优先：额度先满足它写的行数，剩下的才拿去断句。
    #[test]
    fn the_models_own_line_breaks_come_first() {
        let long = "那个报错我刚翻到了 是驱动装岔了版本 你把显卡驱动回退一版再试";
        let Speech::Say(items) = parse(&format!("{long}\n{long}\n{long}"), 3, 12) else {
            panic!("expected speech");
        };
        assert_eq!(items.len(), 3, "三行已经占满额度，不再替它换气");
        let Speech::Say(items) = parse(&format!("{long}\n{long}"), 3, 12) else {
            panic!("expected speech");
        };
        assert_eq!(items.len(), 3, "剩一条额度，给写在前面的那行");
    }

    /// 带 `[at:…]`、`[img:…]` 的行整条发：切开之后那几条会变成另一个意思。
    #[test]
    fn lines_carrying_markup_are_never_cut() {
        let items = say("[at:114514] 这事我刚查过 版本号对不上 你回退一版再试试看行不行");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn waits_are_capped() {
        let items = say("[wait:9999]\n算了");
        assert!(items[0].wait <= MAX_WAIT_SECONDS);
    }


    #[test]
    fn long_lines_type_faster_per_character_but_never_instantly() {
        let pace = Pace {
            typing_cpm: 150,
            voice_cpm: 420,
            think_seconds: 3.0,
        };
        let short = pace.typing_delay(6);
        let long = pace.typing_delay(120);
        assert!(short >= Duration::from_millis(1_200), "{short:?}");
        assert!(long > short, "{long:?} vs {short:?}");
        assert!(pace.effective_cpm(120) > pace.effective_cpm(6));
        // 模型已经想了很久，就不必再假装思考。
        assert_eq!(
            pace.think_delay(Duration::from_secs(30)),
            Duration::from_secs(0)
        );
        assert!(pace.think_delay(Duration::ZERO) > Duration::ZERO);
    }
}
