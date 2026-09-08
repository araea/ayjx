//! 群聊上下文的滚动窗口与每个群的发言节流状态。
//!
//! 搭话不像房间对话那样有明确的一问一答，模型要读的是「刚才这一段群聊」。
//! 窗口只存在内存里：重启后重新攒几条就够用，落库反而要为一个随时会被丢弃的
//! 上下文承担迁移与清理成本。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 每个群保留的消息条数上限。取值比 `context_turns` 宽一些，
/// 好让配置调大时不必等窗口重新攒满。
const WINDOW_CAPACITY: usize = 80;

/// 群聊上下文里的一条消息。
#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub user_id: i64,
    pub name: String,
    pub text: String,
    /// 图片直链，供多模态判定使用。
    pub images: Vec<String>,
    pub message_id: i64,
    /// 是否 @ 了机器人自己。
    pub mentions_me: bool,
    /// 是否是机器人自己说的话。
    pub from_me: bool,
    /// Unix 秒。
    pub at: i64,
}

/// 一个群的上下文与节流状态。
#[derive(Default)]
pub(crate) struct GroupState {
    turns: VecDeque<Turn>,
    /// 收到消息就自增，防抖任务据此判断「还在刷屏」。
    pub seq: u64,
    /// 已经有一个防抖任务在等这个群。
    pub pending: bool,
    /// 正在判定或正在发言。
    pub speaking: bool,
    /// 最近一次发言时刻。
    pub last_spoke: Option<Instant>,
    /// 近期发言时刻，用于每小时上限。
    spoken: VecDeque<Instant>,
}

impl GroupState {
    pub(crate) fn push(&mut self, turn: Turn) {
        if self.turns.len() >= WINDOW_CAPACITY {
            self.turns.pop_front();
        }
        self.turns.push_back(turn);
    }

    /// 最近 `count` 条消息，按时间正序。
    pub(crate) fn recent(&self, count: usize) -> Vec<Turn> {
        self.turns
            .iter()
            .skip(self.turns.len().saturating_sub(count))
            .cloned()
            .collect()
    }

    /// 窗口里最后一条消息是不是自己说的——自言自语要及时打住。
    pub(crate) fn last_is_mine(&self) -> bool {
        self.turns.back().is_some_and(|turn| turn.from_me)
    }

    /// 自己上次说话之后，有没有人 @ 过自己。
    ///
    /// 只看最后一次自己发言之后的消息：三小时前那句「@你」早就不需要回了。
    pub(crate) fn mentioned_since_my_last(&self) -> bool {
        self.turns
            .iter()
            .rev()
            .take_while(|turn| !turn.from_me)
            .any(|turn| turn.mentions_me)
    }

    /// 记一次发言，同时淘汰一小时之前的记录。
    pub(crate) fn mark_spoke(&mut self) {
        let now = Instant::now();
        self.last_spoke = Some(now);
        self.spoken.push_back(now);
        self.prune(now);
    }

    /// 最近一小时内的发言次数。
    pub(crate) fn spoken_last_hour(&mut self) -> usize {
        self.prune(Instant::now());
        self.spoken.len()
    }

    fn prune(&mut self, now: Instant) {
        while let Some(first) = self.spoken.front() {
            if now.duration_since(*first).as_secs() >= 3_600 {
                self.spoken.pop_front();
            } else {
                break;
            }
        }
    }
}

/// 群聊窗口 → 交给模型阅读的聊天记录。
///
/// 带上 QQ 号是为了让回复能 `[at:]` 到人；带上时刻是为了让模型知道哪些话已经
/// 凉了——隔了二十分钟的梗再接就不叫接梗了。
pub(crate) fn transcript(turns: &[Turn]) -> String {
    let mut out = String::new();
    for turn in turns {
        let clock = chrono::DateTime::from_timestamp(turn.at, 0)
            .map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "--:--".to_string());
        let who = if turn.from_me {
            "你自己".to_string()
        } else {
            format!("{}({})", turn.name, turn.user_id)
        };
        out.push_str(&format!("[{clock}] {who}: "));
        out.push_str(turn.text.trim());
        if !turn.images.is_empty() {
            out.push_str(&format!("〔图片 ×{}〕", turn.images.len()));
        }
        if turn.mentions_me {
            out.push_str("〔@了你〕");
        }
        out.push('\n');
    }
    out
}

fn states() -> &'static Mutex<HashMap<i64, GroupState>> {
    static STATES: OnceLock<Mutex<HashMap<i64, GroupState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取出锁访问某个群的状态。闭包里不要 await——锁是同步的。
pub(crate) fn with_group<T>(group_id: i64, action: impl FnOnce(&mut GroupState) -> T) -> T {
    let mut guard = states().lock().unwrap_or_else(|error| error.into_inner());
    action(guard.entry(group_id).or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str, from_me: bool) -> Turn {
        Turn {
            user_id: 1,
            name: "谁".into(),
            text: text.into(),
            images: Vec::new(),
            message_id: 1,
            mentions_me: false,
            from_me,
            at: 0,
        }
    }

    #[test]
    fn window_keeps_the_tail_in_order() {
        let mut state = GroupState::default();
        for index in 0..WINDOW_CAPACITY + 5 {
            state.push(turn(&index.to_string(), false));
        }
        let recent = state.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[2].text, (WINDOW_CAPACITY + 4).to_string());
        assert_eq!(recent[0].text, (WINDOW_CAPACITY + 2).to_string());
        assert!(!state.last_is_mine());
        state.push(turn("我说的", true));
        assert!(state.last_is_mine());
    }

    #[test]
    fn mentions_only_count_after_my_last_words() {
        let mut state = GroupState::default();
        let mut old_mention = turn("在吗", false);
        old_mention.mentions_me = true;
        state.push(old_mention);
        assert!(state.mentioned_since_my_last());
        state.push(turn("嗯", true));
        assert!(!state.mentioned_since_my_last());
        let mut fresh = turn("再问一次", false);
        fresh.mentions_me = true;
        state.push(fresh);
        assert!(state.mentioned_since_my_last());
    }

    #[test]
    fn transcript_names_speakers_and_marks_media() {
        let mut mine = turn("嗯", true);
        mine.at = 1_788_800_000;
        let mut theirs = turn("看这个", false);
        theirs.images = vec!["https://example.com/a.png".into()];
        theirs.mentions_me = true;
        let text = transcript(&[theirs, mine]);
        assert!(text.contains("谁(1): 看这个〔图片 ×1〕〔@了你〕"), "{text}");
        assert!(text.contains("你自己: 嗯"), "{text}");
    }

    #[test]
    fn hourly_counter_tracks_recent_speech() {
        let mut state = GroupState::default();
        assert_eq!(state.spoken_last_hour(), 0);
        state.mark_spoke();
        state.mark_spoke();
        assert_eq!(state.spoken_last_hour(), 2);
        assert!(state.last_spoke.is_some());
    }
}
