//! 状态与兴致：同一句话，凌晨四点和晚上八点不该得到同样的反应。
//!
//! 一个真实的人在群里的存在感是有起伏的——白天精神、午后犯困、半夜迟钝；
//! 刚跟人聊得投机会多说两句，刚说完没人接会消停一会儿。模型本身没有这条曲线，
//! 每次调用都是从零开始的同一个人。这里补上那条曲线：一个随时钟走、随互动
//! 轻微起伏、跨重启保留的内部状态，参与三件事——判定门槛、发言节奏、
//! 以及提示词里那句「你现在什么状态」。
//!
//! 它不编造经历（不会说「我刚下班」），只描述精神头和兴致，这两样是真的
//! 由时间和刚才发生的事决定的。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 精力偏移的半衰期：熬一会儿、被逗一下，效果都是一小时内散掉。
const DRIFT_HALF_LIFE: f32 = 2_400.0;
/// 兴致的半衰期：比精力短得多，聊完一阵就凉。
const WARMTH_HALF_LIFE: f32 = 1_200.0;
/// 兴致的静息值：没什么事发生时的默认热度。
const WARMTH_BASE: f32 = 0.35;

/// 一天里的精神头曲线（按本机小时取）。夜里低、上午回升、晚上最活跃。
fn baseline(hour: u32) -> f32 {
    const CURVE: [f32; 24] = [
        0.55, 0.45, 0.34, 0.22, 0.18, 0.18, 0.25, 0.40, 0.50, 0.60, 0.70, 0.72, 0.60, 0.50, 0.58,
        0.70, 0.75, 0.72, 0.68, 0.80, 0.88, 0.90, 0.85, 0.70,
    ];
    CURVE[(hour % 24) as usize]
}

/// 一段随时间回落的偏移量。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Decaying {
    value: f32,
    at: i64,
}

impl Decaying {
    fn get(self, now: i64, half_life: f32) -> f32 {
        if self.at == 0 || half_life <= 0.0 {
            return 0.0;
        }
        let elapsed = (now - self.at).max(0) as f32;
        self.value * 0.5_f32.powf(elapsed / half_life)
    }

    fn nudge(&mut self, delta: f32, now: i64, half_life: f32, bound: f32) {
        self.value = (self.get(now, half_life) + delta).clamp(-bound, bound);
        self.at = now;
    }
}

/// 人格的内部状态。精力是「他这个人」的，兴致是对每个群分别算的。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Mood {
    /// 相对作息曲线的精力偏移。
    #[serde(default)]
    drift: Decaying,
    /// 每个群的兴致偏移（相对 [`WARMTH_BASE`]）。
    #[serde(default)]
    warmth: HashMap<i64, Decaying>,
}

/// 某一刻的状态快照。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Snapshot {
    /// 0-1，精神头。
    pub energy: f32,
    /// 0-1，对这个群此刻的兴致。
    pub warmth: f32,
}

impl Mood {
    pub(crate) fn snapshot(&self, group: i64, now: i64) -> Snapshot {
        let hour = chrono::DateTime::from_timestamp(now, 0)
            .map(|time| {
                use chrono::Timelike as _;
                time.with_timezone(&chrono::Local).hour()
            })
            .unwrap_or(12);
        let warmth = self
            .warmth
            .get(&group)
            .copied()
            .unwrap_or_default()
            .get(now, WARMTH_HALF_LIFE);
        Snapshot {
            energy: (baseline(hour) + self.drift.get(now, DRIFT_HALF_LIFE)).clamp(0.05, 1.0),
            warmth: (WARMTH_BASE + warmth).clamp(0.0, 1.0),
        }
    }

    /// 刚说完一轮：说话本身耗一点精力，也让人更投入一点。
    pub(crate) fn spoke(&mut self, group: i64, now: i64) {
        self.drift.nudge(-0.03, now, DRIFT_HALF_LIFE, 0.3);
        self.warm(group, 0.10, now);
    }

    /// 被 @、被戳、被人接住话：来劲了。
    pub(crate) fn engaged(&mut self, group: i64, now: i64) {
        self.drift.nudge(0.04, now, DRIFT_HALF_LIFE, 0.3);
        self.warm(group, 0.16, now);
    }

    /// 说完一句没人搭理：兴致降下来，别追着刷存在感。
    pub(crate) fn ignored(&mut self, group: i64, now: i64) {
        self.warm(group, -0.20, now);
    }

    fn warm(&mut self, group: i64, delta: f32, now: i64) {
        self.warmth.entry(group).or_default().nudge(
            delta,
            now,
            WARMTH_HALF_LIFE,
            1.0 - WARMTH_BASE,
        );
    }
}

impl Snapshot {
    /// 一句给模型看的状态。只说精神头和兴致，不编造在干什么。
    pub(crate) fn describe(self) -> String {
        let body = match self.energy {
            e if e < 0.25 => "困得厉害，反应慢半拍，懒得打长句",
            e if e < 0.45 => "没什么劲，能少说就少说",
            e if e < 0.65 => "状态一般，不上不下",
            e if e < 0.85 => "精神不错",
            _ => "精神头很足，手比脑子快",
        };
        let mood = match self.warmth {
            w if w < 0.2 => "对这个群这会儿提不起兴致",
            w if w < 0.4 => "对群里的事没什么特别的兴致",
            w if w < 0.65 => "聊得还行，愿意再看两眼",
            _ => "刚聊得挺顺，还想接着扯",
        };
        format!("你现在的状态：{body}；{mood}。这只是精神头，不是发言配额，也别把它说出来。")
    }

    /// 判定门槛的微调。精神好、兴致高就更容易开口，反之更沉默。
    pub(crate) fn threshold_shift(self) -> i16 {
        let shift = -((self.energy - 0.55) * 14.0 + (self.warmth - WARMTH_BASE) * 12.0);
        shift.round().clamp(-10.0, 12.0) as i16
    }

    /// 精神头对打字速度的影响：困的时候敲得慢，也想得久一点。
    pub(crate) fn typing_scale(self) -> f32 {
        0.65 + 0.55 * self.energy
    }

    pub(crate) fn think_scale(self) -> f32 {
        1.8 - 0.9 * self.energy
    }
}

/// 进程内的状态。`path` 为空时只在内存里活着（测试与未初始化阶段）。
#[derive(Default)]
struct Store {
    path: Option<PathBuf>,
    mood: Mood,
    loaded: bool,
    dirty: bool,
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Store::default()))
}

fn lock() -> MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|error| error.into_inner())
}

/// 指定状态的落盘位置；启动时调用一次。
pub(crate) fn attach(base: &Path) {
    let mut store = lock();
    store.path = Some(base.join("mood.json"));
    store.mood = Mood::default();
    store.loaded = false;
    store.dirty = false;
}

fn ensure_loaded(store: &mut Store) {
    if store.loaded {
        return;
    }
    store.loaded = true;
    if let Some(mood) = store
        .path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<Mood>(&raw).ok())
    {
        store.mood = mood;
    }
}

/// 读当前状态。
pub(crate) fn snapshot(group: i64) -> Snapshot {
    let mut store = lock();
    ensure_loaded(&mut store);
    store.mood.snapshot(group, chrono::Local::now().timestamp())
}

/// 改一下状态，并标记待落盘。
pub(crate) fn nudge(action: impl FnOnce(&mut Mood, i64)) {
    let mut store = lock();
    ensure_loaded(&mut store);
    let now = chrono::Local::now().timestamp();
    action(&mut store.mood, now);
    store.dirty = true;
}

/// 把状态写出去；没有改动时不碰磁盘。
pub(crate) async fn flush() {
    let payload = {
        let mut store = lock();
        if !store.dirty {
            return;
        }
        store.dirty = false;
        store
            .path
            .clone()
            .map(|path| (path, serde_json::to_string(&store.mood).unwrap_or_default()))
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
        warn!(target: "Plugin/OAI", "写入搭话状态失败：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取一个本机时间落在指定小时的时间戳。
    fn at_hour(hour: u32) -> i64 {
        use chrono::{Local, TimeZone as _, Timelike as _};
        let today = Local::now().date_naive();
        Local
            .from_local_datetime(&today.and_hms_opt(hour, 0, 0).unwrap())
            .single()
            .unwrap_or_else(|| Local::now().with_hour(hour).unwrap())
            .timestamp()
    }

    #[test]
    fn the_clock_alone_makes_late_nights_quieter_than_evenings() {
        let mood = Mood::default();
        let night = mood.snapshot(1, at_hour(4));
        let evening = mood.snapshot(1, at_hour(21));
        assert!(night.energy < evening.energy, "{night:?} {evening:?}");
        // 困的时候更沉默、打字更慢、想得更久。
        assert!(night.threshold_shift() > evening.threshold_shift());
        assert!(night.typing_scale() < evening.typing_scale());
        assert!(night.think_scale() > evening.think_scale());
        assert!(night.describe().contains("困"), "{}", night.describe());
    }

    #[test]
    fn interaction_warms_the_room_and_then_it_cools_off() {
        let now = at_hour(20);
        let mut mood = Mood::default();
        let cold = mood.snapshot(1, now).warmth;
        mood.engaged(1, now);
        let warm = mood.snapshot(1, now).warmth;
        assert!(warm > cold, "{warm} vs {cold}");
        // 只影响这个群。
        assert_eq!(mood.snapshot(2, now).warmth, cold);
        // 一小时之后基本凉透。
        let later = mood.snapshot(1, now + 3_600).warmth;
        assert!(later < cold + 0.05, "{later}");
        mood.ignored(1, now + 3_600);
        assert!(mood.snapshot(1, now + 3_600).warmth < cold);
    }

    #[test]
    fn energy_drift_is_bounded_so_the_clock_still_wins() {
        let now = at_hour(21);
        let mut mood = Mood::default();
        for _ in 0..50 {
            mood.spoke(1, now);
        }
        let tired = mood.snapshot(1, now).energy;
        assert!(tired < baseline(21), "{tired}");
        assert!(tired > baseline(21) - 0.31, "{tired}");
        assert!((0.05..=1.0).contains(&tired));
    }

    #[test]
    fn descriptions_stay_about_state_and_never_invent_a_life() {
        for (energy, warmth) in [(0.1, 0.1), (0.5, 0.5), (0.95, 0.95)] {
            let text = Snapshot { energy, warmth }.describe();
            assert!(text.starts_with("你现在的状态："), "{text}");
            assert!(text.contains("不是发言配额"), "{text}");
            assert!(!text.contains("刚下班"), "{text}");
        }
    }

    #[test]
    fn a_mood_survives_a_round_trip_through_json() {
        let now = at_hour(15);
        let mut mood = Mood::default();
        mood.engaged(7, now);
        let raw = serde_json::to_string(&mood).unwrap();
        let back: Mood = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.snapshot(7, now), mood.snapshot(7, now));
        // 旧文件缺字段也读得回来。
        assert!(serde_json::from_str::<Mood>("{}").is_ok());
    }
}
