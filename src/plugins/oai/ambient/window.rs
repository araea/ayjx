//! 群聊上下文的滚动窗口与每个群的发言节流状态。
//!
//! 搭话不像房间对话那样有明确的一问一答，模型要读的是「刚才这一段群聊」。
//! 窗口只存在内存里：重启后重新攒几条就够用，落库反而要为一个随时会被丢弃的
//! 上下文承担迁移与清理成本。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::attention::Focus;

/// 每个群保留的消息条数上限。取值比 `context_turns` 宽一些，
/// 好让配置调大时不必等窗口重新攒满。
const WINDOW_CAPACITY: usize = 80;

/// 「刚才说了几轮」的观察窗口。群聊的节奏以十分钟为单位看正合适：
/// 再短看不出是不是一直在接话，再长又会把半小时前的事算到现在头上。
pub(crate) const RECENT_SPEECH: std::time::Duration = std::time::Duration::from_secs(600);

/// 群聊上下文里的一条消息。
#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub user_id: i64,
    pub name: String,
    pub text: String,
    /// 图片直链，供多模态判定使用。
    pub images: Vec<String>,
    /// 保留资源和引用参数，供工具按消息 ID 复用。
    pub elements: crate::message::Message,
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
    pub running: bool,
    /// 只消费本批次的点名；人格选择沉默后不反复拿旧 @ 强制唤醒。
    unread_mention: bool,
    /// 本批次收到过搭话指令；与点名一样只消费一次。
    unread_summon: bool,
    pub focus: Option<Focus>,
    /// 最近一次发言时刻。
    pub last_spoke: Option<Instant>,
    /// 自己上次开口的墙上时刻（Unix 秒），等着看多久有人接。
    spoke_at: Option<i64>,
    /// 一次性的反馈：上次开口之后隔了多少秒才有人再说话。
    feedback: Option<i64>,
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
    #[cfg(test)]
    fn last_is_mine(&self) -> bool {
        self.turns.back().is_some_and(|turn| turn.from_me)
    }

    /// 收消息与占用 worker 必须在同一把锁下完成，避免交接时漏消息。
    pub(crate) fn receive(&mut self, turn: Turn) -> bool {
        if turn.message_id != 0
            && self
                .turns
                .iter()
                .any(|old| old.message_id == turn.message_id)
        {
            return false;
        }
        let incoming = !turn.from_me;
        if incoming {
            self.seq += 1;
            self.unread_mention |= turn.mentions_me;
            // 说完之后第一个开口的人，决定这次发言是被接住了还是掉地上了。
            if let Some(spoke_at) = self.spoke_at.take() {
                self.feedback = Some((turn.at - spoke_at).max(0));
            }
        }
        self.push(turn);
        if !incoming || self.running {
            return false;
        }
        self.running = true;
        true
    }

    pub(crate) fn recall(&mut self, id: i64) {
        if let Some(turn) = self
            .turns
            .iter_mut()
            .find(|turn| turn.message_id == id && id != 0)
        {
            turn.text = "[消息已撤回]".into();
            turn.images.clear();
            turn.elements = crate::message::Message::new();
            // 不再允许引用、转发或再次撤回这个 ID。
            turn.message_id = 0;
        }
    }

    pub(crate) fn take_mention(&mut self) -> bool {
        std::mem::take(&mut self.unread_mention)
    }

    /// 收到一条搭话指令。
    ///
    /// 指令本身没有内容可给模型看，所以它不进窗口，只说明「这一批欠一句回应」；
    /// 若那条消息还带着正文，由调用方按普通消息另行 `receive`。
    /// 返回 true 表示这个群现在没有 worker，调用方去跑一次 `consider`。
    pub(crate) fn summon(&mut self) -> bool {
        self.seq += 1;
        self.unread_summon = true;
        if self.running {
            return false;
        }
        self.running = true;
        true
    }

    pub(crate) fn take_summon(&mut self) -> bool {
        std::mem::take(&mut self.unread_summon)
    }

    /// 新消息即使出现在模型执行或发送期间，也由同一 worker 接着处理。
    pub(crate) fn finish_batch(&mut self, processed: u64) -> bool {
        if self.seq != processed {
            true
        } else {
            self.running = false;
            false
        }
    }

    pub(crate) fn active_focus(&self) -> Option<&Focus> {
        self.focus
            .as_ref()
            .filter(|focus| focus.until > Instant::now())
    }

    pub(crate) fn rhythm(&mut self) -> String {
        let count = self.spoken_last_hour();
        let recent = self.spoken_within(RECENT_SPEECH);
        let since = self.last_spoke.map_or("尚未发言".to_string(), |at| {
            format!("{} 秒前发过言", at.elapsed().as_secs())
        });
        let focus = self
            .active_focus()
            .map_or("无，按兴趣旁观".to_string(), |focus| {
                format!(
                    "群友 {:?}；话题 {}；还关注 {} 秒",
                    focus.users,
                    focus.topic,
                    focus
                        .until
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                )
            });
        let crowding = match recent {
            0 => "",
            1 => "刚接过一轮，这一轮交给别人也正好。",
            _ => "最近这十分钟已经由你说了好几轮，这会儿看着就好。",
        };
        format!(
            "你{since}，最近十分钟发言 {recent} 轮，近一小时 {count} 轮。{crowding}\
             当前关注：{focus}。关注是给自己留个念想，接不接随你；有新意又还在继续的对话最值得接。"
        )
    }

    pub(crate) fn is_own_message(&self, id: i64) -> bool {
        id != 0
            && self
                .turns
                .iter()
                .any(|turn| turn.from_me && turn.message_id == id)
    }

    /// 取走「上次开口多久才有人接」，只取一次。
    pub(crate) fn take_feedback(&mut self) -> Option<i64> {
        self.feedback.take()
    }

    /// 记一次发言，同时淘汰一小时之前的记录。
    pub(crate) fn mark_spoke(&mut self) {
        let now = Instant::now();
        self.last_spoke = Some(now);
        self.spoke_at = Some(chrono::Local::now().timestamp());
        self.feedback = None;
        self.spoken.push_back(now);
        self.prune(now);
    }

    /// 最近一小时内的发言次数。
    pub(crate) fn spoken_last_hour(&mut self) -> usize {
        self.prune(Instant::now());
        self.spoken.len()
    }

    /// 最近这段时间里说了几轮。刚说过好几句的人本来就该消停一会儿。
    pub(crate) fn spoken_within(&self, window: std::time::Duration) -> usize {
        let now = Instant::now();
        self.spoken
            .iter()
            .filter(|at| now.duration_since(**at) < window)
            .count()
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
        let who = if turn.from_me && turn.user_id == 0 {
            "平台事件（操作者未知）".to_string()
        } else if turn.from_me {
            "你自己".to_string()
        } else {
            format!("{}({})", turn.name, turn.user_id)
        };
        out.push_str(&format!("[{clock} id={}] {who}: ", turn.message_id));
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
    use std::time::Duration;

    fn turn(text: &str, from_me: bool) -> Turn {
        Turn {
            user_id: 1,
            name: "谁".into(),
            text: text.into(),
            elements: crate::message::Message::new(),
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
    fn worker_drains_messages_arriving_during_a_reply_and_consumes_mentions_once() {
        let mut state = GroupState::default();
        let mut first = turn("在吗", false);
        first.mentions_me = true;
        assert!(state.receive(first));
        assert!(state.take_mention());
        let snapshot = state.seq;
        let mut second = turn("接着聊", false);
        second.message_id = 2;
        assert!(!state.receive(second.clone()));
        assert!(!state.receive(second)); // 重复事件不唤醒
        assert_eq!(state.seq, snapshot + 1);
        state.push(turn("刚生成的回复", true));
        assert!(state.last_is_mine());
        assert!(state.finish_batch(snapshot)); // 即使自己的回填在最后，也不能漏掉新消息
        assert!(!state.take_mention());
        assert!(!state.finish_batch(state.seq));
        assert!(!state.running);
        let mut third = turn("新一轮", false);
        third.message_id = 3;
        assert!(state.receive(third));
    }

    /// 搭话指令不进窗口，但它必须能把一个闲着的群叫起来，且只叫一次。
    #[test]
    fn a_summon_wakes_an_idle_group_once_without_taking_a_turn() {
        let mut state = GroupState::default();
        assert!(!state.take_summon());
        assert!(state.summon());
        assert!(state.running);
        assert_eq!(state.seq, 1);
        assert!(state.turns.is_empty());
        assert!(state.take_summon());
        // 一次指令只算一次；人格沉默之后不会拿旧指令再唤醒。
        assert!(!state.take_summon());
        // 已经有 worker 在跑时只留下指令，由它下一轮自己看见。
        assert!(!state.summon());
        assert_eq!(state.seq, 2);
        assert!(state.take_summon());
    }

    #[test]
    fn focus_expires_and_never_wakes_itself() {
        let mut state = GroupState::default();
        state.focus = Some(Focus {
            users: vec![1],
            topic: "游戏".into(),
            until: Instant::now() + Duration::from_secs(30),
        });
        assert!(state.active_focus().is_some());
        assert!(!state.running);
        state.focus.as_mut().unwrap().until = Instant::now() - Duration::from_secs(1);
        assert!(state.active_focus().is_none());
        assert!(state.rhythm().contains("无，按兴趣旁观"));
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
    fn recall_erases_content_and_media_and_invalidates_target() {
        let mut state = GroupState::default();
        let mut t = turn("不再显示", true);
        t.images.push("https://example.com/private.png".into());
        t.elements = crate::message::Message::new().text("不再显示");
        state.receive(t);
        state.recall(1);
        let t = &state.recent(1)[0];
        assert_eq!(t.text, "[消息已撤回]");
        assert!(t.images.is_empty());
        assert!(t.elements.0.is_empty());
        assert!(!state.is_own_message(1));
        assert_eq!(t.message_id, 0);
    }

    #[test]
    fn how_long_the_room_took_to_answer_is_reported_exactly_once() {
        let mut state = GroupState::default();
        assert_eq!(state.take_feedback(), None);
        state.mark_spoke();
        let spoke_at = state.spoke_at.unwrap();
        let mut reply = turn("哦", false);
        reply.message_id = 9;
        reply.at = spoke_at + 12;
        state.receive(reply);
        assert_eq!(state.take_feedback(), Some(12));
        // 反馈只算一次，后面的消息不再反复给同一次发言打分。
        let mut later = turn("再说一句", false);
        later.message_id = 10;
        later.at = spoke_at + 600;
        state.receive(later);
        assert_eq!(state.take_feedback(), None);
    }

    #[test]
    fn hourly_counter_tracks_recent_speech() {
        let mut state = GroupState::default();
        assert_eq!(state.spoken_last_hour(), 0);
        assert_eq!(state.spoken_within(RECENT_SPEECH), 0);
        state.mark_spoke();
        state.mark_spoke();
        assert_eq!(state.spoken_last_hour(), 2);
        // 刚说的两轮当然落在最近十分钟里，节奏描述也要让模型看见这件事。
        assert_eq!(state.spoken_within(RECENT_SPEECH), 2);
        assert_eq!(state.spoken_within(Duration::ZERO), 0);
        let rhythm = state.rhythm();
        assert!(rhythm.contains("最近十分钟发言 2 轮"), "{rhythm}");
        assert!(rhythm.contains("看着就好"), "{rhythm}");
        assert!(state.last_spoke.is_some());
    }
}
