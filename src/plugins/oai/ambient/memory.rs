//! 熟人记忆：跨重启记住群里的人和这个群里发生过的事。
//!
//! 滚动窗口（[`super::window`]）只有最近几十条消息，重启就没了——那是「刚才」，
//! 不是「认识」。真正让一个群友显得像常驻的人，是他记得你上次修的那台破电脑、
//! 记得这个群三天前开始玩的梗。这里就存这一点点东西：每个人一句印象、每个群
//! 几条旧事，落在磁盘上，随时间自然淡忘。
//!
//! 记忆只在两处产生：见到消息时自动更新的露面统计（不花钱），以及人格自己
//! 通过 `satori_memo` 写下的一句话（它自己决定什么值得记）。两者都不调用模型。

use super::window::Turn;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

/// 每个群最多记住多少人；超出时先忘掉没有印象、最久没露面的那些。
const MAX_PEOPLE: usize = 120;
/// 每个群最多记住多少条旧事。
const MAX_NOTES: usize = 24;
/// 旧事的保鲜期；过了就自然淡忘，免得半年前的梗还挂在嘴边。
const NOTE_TTL_DAYS: i64 = 45;
/// 一条印象或旧事的字数上限——记忆是提示，不是日记。
pub(crate) const MAX_NOTE_CHARS: usize = 60;
/// 一次注入提示词的熟人卡片上限。
const BRIEF_PEOPLE: usize = 8;
/// 一次注入提示词的旧事条数上限。
const BRIEF_NOTES: usize = 6;
/// 两次落盘之间至少隔多久。露面统计每条消息都在变，值不上一次写盘；
/// 人格自己写下的印象走 [`flush_now`]，不受这个节流影响。
const WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// 对一个人的记忆。
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Person {
    /// 最近一次见到的群名片。
    #[serde(default)]
    pub name: String,
    /// 人格自己写的一句印象；空表示见过但没什么印象。
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub first_seen: i64,
    #[serde(default)]
    pub last_seen: i64,
    /// 见过他发的消息条数。
    #[serde(default)]
    pub messages: u32,
    /// 自己回应过他多少次。
    #[serde(default)]
    pub exchanges: u32,
}

impl Person {
    /// 刚冒头的新面孔：见过的话不多，第一次露面也没多久。
    fn stranger(&self, now: i64) -> bool {
        self.note.is_empty() && self.messages < 6 && now - self.first_seen < 3 * 86_400
    }
}

/// 群里的一件旧事：梗、共同话题、约定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Note {
    pub text: String,
    pub at: i64,
}

/// 一个群的全部长期记忆。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct GroupMemory {
    #[serde(default)]
    pub people: HashMap<i64, Person>,
    #[serde(default)]
    pub notes: Vec<Note>,
}

/// 把一句印象裁到能进提示词的长度，并压平换行。
fn tidy(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_NOTE_CHARS)
        .collect()
}

/// 「多久以前」的口语说法；记忆里的时间感比精确时刻有用。
pub(crate) fn ago(seconds: i64) -> String {
    match seconds {
        ..=59 => "刚刚".to_string(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        86_400..=2_591_999 => format!("{} 天前", seconds / 86_400),
        _ => "很久以前".to_string(),
    }
}

impl GroupMemory {
    /// 见到某人发了一条消息。只更新统计，不花任何模型开销。
    pub(crate) fn see(&mut self, user_id: i64, name: &str, at: i64) {
        if user_id <= 0 {
            return;
        }
        let person = self.people.entry(user_id).or_insert_with(|| Person {
            first_seen: at,
            ..Person::default()
        });
        if !name.trim().is_empty() {
            person.name = name.trim().chars().take(32).collect();
        }
        if person.first_seen == 0 {
            person.first_seen = at;
        }
        person.last_seen = person.last_seen.max(at);
        person.messages = person.messages.saturating_add(1);
    }

    /// 自己回应了某人一次。
    pub(crate) fn exchange(&mut self, user_id: i64, at: i64) {
        if let Some(person) = self.people.get_mut(&user_id) {
            person.exchanges = person.exchanges.saturating_add(1);
            person.last_seen = person.last_seen.max(at);
        }
    }

    /// 写下（或改写）对一个人的印象；空字符串等于把印象抹掉但仍认得这个人。
    pub(crate) fn remember(&mut self, user_id: i64, note: &str) -> anyhow::Result<()> {
        anyhow::ensure!(user_id > 0, "QQ 号无效");
        let person = self.people.entry(user_id).or_default();
        person.note = tidy(note);
        Ok(())
    }

    /// 彻底忘掉一个人。
    pub(crate) fn forget(&mut self, user_id: i64) -> bool {
        self.people.remove(&user_id).is_some()
    }

    /// 记下群里的一件事。重复的旧事只刷新时间，不堆成一摞。
    pub(crate) fn jot(&mut self, text: &str, at: i64) -> anyhow::Result<()> {
        let text = tidy(text);
        anyhow::ensure!(!text.is_empty(), "要记的内容是空的");
        if let Some(existing) = self.notes.iter_mut().find(|note| note.text == text) {
            existing.at = at;
            return Ok(());
        }
        self.notes.push(Note { text, at });
        Ok(())
    }

    /// 忘掉一件旧事；按内容前缀匹配，人格记不住下标。
    pub(crate) fn drop_note(&mut self, needle: &str) -> bool {
        let needle = tidy(needle);
        if needle.is_empty() {
            return false;
        }
        let before = self.notes.len();
        self.notes
            .retain(|note| !note.text.contains(needle.as_str()));
        self.notes.len() != before
    }

    /// 淡忘：过期的旧事、太多的人。有印象的人比路人先留下。
    pub(crate) fn prune(&mut self, now: i64) {
        let ttl = NOTE_TTL_DAYS * 86_400;
        self.notes.retain(|note| now - note.at < ttl);
        if self.notes.len() > MAX_NOTES {
            self.notes.sort_by_key(|note| note.at);
            let excess = self.notes.len() - MAX_NOTES;
            self.notes.drain(..excess);
        }
        if self.people.len() > MAX_PEOPLE {
            let mut ranked: Vec<(i64, i64, bool)> = self
                .people
                .iter()
                .map(|(id, person)| (*id, person.last_seen, person.note.is_empty()))
                .collect();
            // 先淘汰没有印象的，再按最久没露面淘汰。
            ranked.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
            for (id, _, _) in ranked.into_iter().take(self.people.len() - MAX_PEOPLE) {
                self.people.remove(&id);
            }
        }
    }

    /// 当前这段聊天里出现的人 + 群里的旧事 → 注入提示词的一段话。
    ///
    /// 只列眼前这些人：把整本通讯录倒进上下文既贵又没用，人也不是那样想事情的。
    pub(crate) fn brief(&self, turns: &[Turn], now: i64) -> String {
        let mut seen: Vec<i64> = Vec::new();
        for turn in turns.iter().rev() {
            if turn.from_me || turn.user_id <= 0 || seen.contains(&turn.user_id) {
                continue;
            }
            seen.push(turn.user_id);
            if seen.len() >= BRIEF_PEOPLE {
                break;
            }
        }
        let mut out = String::new();
        let mut cards = Vec::new();
        for id in seen {
            let Some(person) = self.people.get(&id) else {
                continue;
            };
            let name = if person.name.is_empty() {
                id.to_string()
            } else {
                format!("{}({id})", person.name)
            };
            if person.stranger(now) {
                cards.push(format!("- {name}：新面孔，之前没打过交道"));
                continue;
            }
            let mut facts = Vec::new();
            if person.exchanges > 0 {
                facts.push(format!("聊过 {} 次", person.exchanges));
            }
            if person.last_seen > 0 && now > person.last_seen {
                facts.push(format!("上次露面 {}", ago(now - person.last_seen)));
            }
            let tail = if facts.is_empty() {
                String::new()
            } else {
                format!("（{}）", facts.join("，"))
            };
            let note = if person.note.is_empty() {
                "眼熟，没什么具体印象".to_string()
            } else {
                person.note.clone()
            };
            cards.push(format!("- {name}：{note}{tail}"));
        }
        if !cards.is_empty() {
            out.push_str("你记得的人：\n");
            out.push_str(&cards.join("\n"));
            out.push('\n');
        }
        let mut notes: Vec<&Note> = self.notes.iter().collect();
        notes.sort_by_key(|note| std::cmp::Reverse(note.at));
        let notes: Vec<String> = notes
            .into_iter()
            .take(BRIEF_NOTES)
            .map(|note| format!("- {}（{}）", note.text, ago((now - note.at).max(0))))
            .collect();
        if !notes.is_empty() {
            out.push_str("这个群的旧事：\n");
            out.push_str(&notes.join("\n"));
            out.push('\n');
        }
        out
    }

    /// 给工具回执用的一句话概况。
    pub(crate) fn summary(&self) -> String {
        let with_note = self.people.values().filter(|p| !p.note.is_empty()).count();
        format!(
            "记得 {} 个人（{} 个有印象），{} 条旧事",
            self.people.len(),
            with_note,
            self.notes.len()
        )
    }
}

/// 进程内的记忆总账。`dir` 为空时只在内存里活着（测试与未初始化阶段）。
#[derive(Default)]
struct Store {
    dir: Option<PathBuf>,
    groups: HashMap<i64, GroupMemory>,
    dirty: HashSet<i64>,
    written: HashMap<i64, Instant>,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

fn lock() -> MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|error| error.into_inner())
}

fn path_of(dir: &Path, group: i64) -> PathBuf {
    dir.join(format!("{group}.json"))
}

/// 指定记忆的落盘位置；启动时调用一次。
pub(crate) fn attach(base: &Path) {
    let mut store = lock();
    store.dir = Some(base.join("memory"));
    store.groups.clear();
    store.dirty.clear();
    store.written.clear();
}

/// 取出某个群的记忆做一次修改。闭包里不要 await——锁是同步的。
pub(crate) fn with_group<T>(group: i64, action: impl FnOnce(&mut GroupMemory) -> T) -> T {
    let mut store = lock();
    if !store.groups.contains_key(&group) {
        let loaded = store
            .dir
            .as_ref()
            .and_then(|dir| std::fs::read_to_string(path_of(dir, group)).ok())
            .and_then(|raw| serde_json::from_str::<GroupMemory>(&raw).ok())
            .unwrap_or_default();
        store.groups.insert(group, loaded);
    }
    action(store.groups.get_mut(&group).expect("just inserted"))
}

/// 同上，但顺带标记「有改动，待落盘」。
pub(crate) fn edit<T>(group: i64, action: impl FnOnce(&mut GroupMemory) -> T) -> T {
    let result = with_group(group, action);
    lock().dirty.insert(group);
    result
}

/// 把待落盘的改动写出去，最多每 [`WRITE_INTERVAL`] 一次。没有改动时不碰磁盘。
pub(crate) async fn flush(group: i64) {
    write(group, false).await
}

/// 立刻落盘，不受节流限制：人格刚写下的印象值得马上留住。
pub(crate) async fn flush_now(group: i64) {
    write(group, true).await
}

async fn write(group: i64, force: bool) {
    let payload = {
        let mut store = lock();
        if !store.dirty.contains(&group) {
            return;
        }
        if !force
            && store
                .written
                .get(&group)
                .is_some_and(|at| at.elapsed() < WRITE_INTERVAL)
        {
            return;
        }
        store.dirty.remove(&group);
        store.written.insert(group, Instant::now());
        let Some(dir) = store.dir.clone() else {
            return;
        };
        store.groups.get_mut(&group).map(|memory| {
            memory.prune(chrono::Local::now().timestamp());
            (
                path_of(&dir, group),
                serde_json::to_string(memory).unwrap_or_default(),
            )
        })
    };
    let Some((path, json)) = payload else {
        return;
    };
    if json.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Err(error) = tokio::fs::write(&path, json).await {
        warn!(target: "Plugin/OAI", "写入群 {group} 的搭话记忆失败：{error}");
    }
}

/// 全局记忆是进程级的，几个测试都要动它；用一把锁把它们串起来。
#[cfg(test)]
pub(crate) fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(user_id: i64, name: &str) -> Turn {
        Turn {
            user_id,
            name: name.into(),
            text: "在的".into(),
            images: vec![],
            elements: crate::message::Message::new(),
            message_id: user_id,
            mentions_me: false,
            from_me: false,
            at: 0,
        }
    }

    #[test]
    fn seeing_someone_builds_a_profile_without_inventing_an_impression() {
        let mut memory = GroupMemory::default();
        memory.see(42, "老张", 1_000);
        memory.see(42, "老张", 2_000);
        let person = &memory.people[&42];
        assert_eq!(person.messages, 2);
        assert_eq!(person.first_seen, 1_000);
        assert_eq!(person.last_seen, 2_000);
        assert!(person.note.is_empty());
        // 刚见过几面的人在提示词里是「新面孔」，不装熟。
        let brief = memory.brief(&[turn(42, "老张")], 2_000);
        assert!(brief.contains("新面孔"), "{brief}");
    }

    #[test]
    fn impressions_are_trimmed_flattened_and_replaceable() {
        let mut memory = GroupMemory::default();
        memory.see(42, "老张", 0);
        memory
            .remember(42, &format!("修车的\n{}", "很".repeat(200)))
            .unwrap();
        let note = memory.people[&42].note.clone();
        assert!(note.chars().count() <= MAX_NOTE_CHARS);
        assert!(!note.contains('\n'));
        memory.remember(42, "改行卖鱼了").unwrap();
        assert_eq!(memory.people[&42].note, "改行卖鱼了");
        assert!(memory.remember(0, "x").is_err());
        assert!(memory.forget(42));
        assert!(!memory.forget(42));
    }

    #[test]
    fn notes_deduplicate_refresh_and_expire() {
        let now = 30 * 86_400;
        let mut memory = GroupMemory::default();
        memory.jot("全自动坐牢", 10).unwrap();
        memory.jot("全自动坐牢", 20).unwrap();
        assert_eq!(memory.notes.len(), 1);
        assert_eq!(memory.notes[0].at, 20);
        assert!(memory.jot("   ", 20).is_err());
        memory.jot("老张的破笔记本", now).unwrap();
        memory.prune(now + NOTE_TTL_DAYS * 86_400 - 1);
        assert_eq!(memory.notes.len(), 1);
        assert_eq!(memory.notes[0].text, "老张的破笔记本");
        assert!(memory.drop_note("破笔记本"));
        assert!(memory.notes.is_empty());
        assert!(!memory.drop_note("破笔记本"));
    }

    #[test]
    fn pruning_keeps_the_people_worth_remembering() {
        let mut memory = GroupMemory::default();
        for id in 1..=(MAX_PEOPLE as i64 + 20) {
            memory.see(id, "路人", id);
        }
        memory.remember(1, "最早认识的那个").unwrap();
        memory.prune(1_000_000);
        assert_eq!(memory.people.len(), MAX_PEOPLE);
        // 有印象的人即使最久没露面也留着；没印象的路人先被忘掉。
        assert!(memory.people.contains_key(&1));
        assert!(!memory.people.contains_key(&2));
    }

    #[test]
    fn the_brief_only_covers_people_in_the_room_and_recent_lore() {
        let mut memory = GroupMemory::default();
        for id in [42, 43, 44] {
            for _ in 0..20 {
                memory.see(id, &format!("群友{id}"), 0);
            }
        }
        memory.remember(42, "在修驾校那台破电脑").unwrap();
        memory.exchange(42, 100);
        memory.jot("上周开始玩的梗", 0).unwrap();
        let brief = memory.brief(&[turn(42, "群友42"), turn(43, "群友43")], 86_400);
        assert!(brief.contains("在修驾校那台破电脑"), "{brief}");
        assert!(brief.contains("聊过 1 次"), "{brief}");
        assert!(brief.contains("群友43(43)：眼熟"), "{brief}");
        assert!(!brief.contains("群友44"), "{brief}");
        assert!(brief.contains("上周开始玩的梗"), "{brief}");
        assert!(brief.contains("1 天前"), "{brief}");
        assert!(GroupMemory::default().brief(&[turn(42, "谁")], 0).is_empty());
    }

    #[test]
    fn elapsed_time_reads_like_a_person_would_say_it() {
        assert_eq!(ago(0), "刚刚");
        assert_eq!(ago(59), "刚刚");
        assert_eq!(ago(600), "10 分钟前");
        assert_eq!(ago(7_200), "2 小时前");
        assert_eq!(ago(3 * 86_400), "3 天前");
        assert_eq!(ago(400 * 86_400), "很久以前");
    }

    // 这把锁只是把动全局记忆的几个测试串起来，跨 await 持有正是它的用途。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn attaching_a_directory_makes_the_memory_outlive_the_process() {
        let _guard = exclusive();
        let base = std::env::temp_dir().join(format!("ayjx-memory-{}", rand::random::<u64>()));
        attach(&base);
        let now = chrono::Local::now().timestamp();
        edit(7, |memory| {
            memory.see(42, "老张", now);
            memory.remember(42, "在修驾校那台破电脑").unwrap();
        });
        flush(7).await;
        let path = path_of(&base.join("memory"), 7);
        assert!(path.exists(), "{path:?}");

        // 刚写过就再改，普通 flush 让位给节流；人格自己写下的印象立刻落盘。
        edit(7, |memory| memory.jot("刚起的梗", now).unwrap());
        flush(7).await;
        assert!(!std::fs::read_to_string(&path).unwrap().contains("刚起的梗"));
        flush_now(7).await;
        assert!(std::fs::read_to_string(&path).unwrap().contains("刚起的梗"));

        // 没有改动就不碰磁盘。
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        flush_now(7).await;
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);

        // 重新 attach 等于重启一次：还认得这个人，也记得那个梗。
        attach(&base);
        assert_eq!(
            with_group(7, |memory| memory.people[&42].note.clone()),
            "在修驾校那台破电脑"
        );
        assert_eq!(with_group(7, |memory| memory.notes[0].text.clone()), "刚起的梗");

        attach(&std::env::temp_dir().join("ayjx-memory-detached"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn memory_survives_a_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("ayjx-memory-{}", rand::random::<u64>()));
        let mut memory = GroupMemory::default();
        memory.see(42, "老张", 100);
        memory.remember(42, "修电脑的").unwrap();
        memory.jot("旧事一件", 100).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_of(&dir, -1);
        std::fs::write(&path, serde_json::to_string(&memory).unwrap()).unwrap();
        let back: GroupMemory =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.people[&42].note, "修电脑的");
        assert_eq!(back.notes[0].text, "旧事一件");
        assert!(back.summary().contains("1 个有印象"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
