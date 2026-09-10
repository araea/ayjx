//! 按 DeepSeek 的计价时段决定这会儿值不值得开口。
//!
//! DeepSeek 官方接口把「北京时间周一至周五 9:00–12:00、14:00–18:00」定为高峰，
//! 其余时间——午休、傍晚、整夜、整个周末——都是空闲时段，价格是高峰的一半。
//!
//! 搭话是这个 bot 里唯一一个无人触发、跟着群消息频率自动跑的付费功能，所以它
//! 也是最值得挑时段的那个：把它压回空闲时段，账单直接对折，而群里最热闹的晚上
//! 本来就落在空闲时段里，几乎不损失什么。高峰时段有两种省法——彻底不出声，
//! 或者睡着：不再主动判定（判定是最频繁的那次调用），只有被点名才醒一次，
//! 并且用最省的一份上下文。
//!
//! 时段写在配置里而不是写死：定价规则会变，改一行配置比改一次编译便宜。

use serde::{Deserialize, Serialize};

/// 高峰时段的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// 不理会时段，照常。
    Normal,
    /// 睡着：不主动判定，被 @ / 引用 / 戳一戳才醒一次，且用最省的上下文。
    Sleep,
    /// 停用：高峰时段一句话都不说。
    Pause,
}

/// 这会儿该以什么姿态待着。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stance {
    /// 空闲时段（或关掉了时段管理）：照常。
    Awake,
    /// 高峰时段，睡着：只有被点名才回应。
    Dozing,
    /// 高峰时段，停用。
    Asleep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PeakConfig {
    /// 高峰时段怎么办。
    pub mode: Mode,
    /// 高峰时段，`HH:MM-HH:MM`，按本机时间，可跨零点；空列表等于没有高峰。
    pub windows: Vec<String>,
    /// 算作高峰的星期几，1=周一 … 7=周日；空列表等于每天都算。
    pub weekdays: Vec<u8>,
}

impl Default for PeakConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Sleep,
            windows: vec!["09:00-12:00".to_string(), "14:00-18:00".to_string()],
            weekdays: vec![1, 2, 3, 4, 5],
        }
    }
}

/// `HH:MM-HH:MM` → 一天中的起止分钟数。写坏的时段当作不存在，不去猜它想表达什么。
fn parse_window(window: &str) -> Option<(u32, u32)> {
    let (start, end) = window.split_once('-')?;
    Some((parse_clock(start)?, parse_clock(end)?))
}

fn parse_clock(clock: &str) -> Option<u32> {
    let (hour, minute) = clock.trim().split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

impl PeakConfig {
    /// 这个时刻算不算高峰。跨零点的时段按当时那一刻的星期几判断。
    pub(crate) fn is_peak_at(&self, at: chrono::DateTime<chrono::Local>) -> bool {
        use chrono::{Datelike as _, Timelike as _};
        let weekday = at.weekday().number_from_monday() as u8;
        if !self.weekdays.is_empty() && !self.weekdays.contains(&weekday) {
            return false;
        }
        let minutes = at.hour() * 60 + at.minute();
        self.windows
            .iter()
            .filter_map(|window| parse_window(window))
            .any(|(start, end)| {
                if start <= end {
                    minutes >= start && minutes < end
                } else {
                    minutes >= start || minutes < end
                }
            })
    }

    pub(crate) fn stance_at(&self, at: chrono::DateTime<chrono::Local>) -> Stance {
        if self.mode == Mode::Normal || !self.is_peak_at(at) {
            return Stance::Awake;
        }
        match self.mode {
            Mode::Pause => Stance::Asleep,
            _ => Stance::Dozing,
        }
    }

    pub(crate) fn stance(&self) -> Stance {
        self.stance_at(chrono::Local::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    /// 2026-09-10 是周四；给一个本机时刻。
    fn thursday(hour: u32, minute: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 9, 10, hour, minute, 0)
            .single()
            .expect("本机时区里这个时刻存在")
    }

    fn sunday(hour: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 9, 13, hour, 0, 0)
            .single()
            .expect("本机时区里这个时刻存在")
    }

    #[test]
    fn deepseek_peak_hours_cover_the_working_day_and_nothing_else() {
        let config = PeakConfig::default();
        // 高峰：工作日上午与下午的两段。
        assert!(config.is_peak_at(thursday(9, 0)));
        assert!(config.is_peak_at(thursday(11, 59)));
        assert!(config.is_peak_at(thursday(17, 59)));
        // 空闲：午休、傍晚、深夜、整个周末。
        assert!(!config.is_peak_at(thursday(8, 59)));
        assert!(!config.is_peak_at(thursday(12, 0)));
        assert!(!config.is_peak_at(thursday(18, 0)));
        assert!(!config.is_peak_at(thursday(2, 0)));
        assert!(!config.is_peak_at(sunday(10)));
    }

    #[test]
    fn the_mode_decides_what_peak_hours_mean() {
        let mut config = PeakConfig::default();
        assert_eq!(config.stance_at(thursday(10, 0)), Stance::Dozing);
        assert_eq!(config.stance_at(thursday(20, 0)), Stance::Awake);
        config.mode = Mode::Pause;
        assert_eq!(config.stance_at(thursday(10, 0)), Stance::Asleep);
        assert_eq!(config.stance_at(sunday(10)), Stance::Awake);
        // 关掉时段管理之后，高峰时段也照常。
        config.mode = Mode::Normal;
        assert_eq!(config.stance_at(thursday(10, 0)), Stance::Awake);
    }

    #[test]
    fn windows_are_configurable_and_a_broken_one_is_ignored_rather_than_guessed() {
        let config = PeakConfig {
            mode: Mode::Pause,
            // 跨零点的时段；另一条写坏了。
            windows: vec!["23:00-02:00".to_string(), "上午".to_string()],
            weekdays: vec![],
        };
        assert!(config.is_peak_at(thursday(23, 30)));
        assert!(config.is_peak_at(thursday(1, 0)));
        assert!(!config.is_peak_at(thursday(3, 0)));
        // weekdays 留空等于每天都算。
        assert!(config.is_peak_at(sunday(23)));
        // 没有时段就没有高峰。
        let none = PeakConfig {
            windows: vec![],
            ..PeakConfig::default()
        };
        assert!(!none.is_peak_at(thursday(10, 0)));
    }

    #[test]
    fn config_round_trips_through_toml_and_old_files_get_the_defaults() {
        let config: PeakConfig = toml::from_str("").unwrap();
        assert_eq!(config.mode, Mode::Sleep);
        let text = toml::to_string(&PeakConfig {
            mode: Mode::Pause,
            ..PeakConfig::default()
        })
        .unwrap();
        assert!(text.contains("\"pause\""), "{text}");
        let back: PeakConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.mode, Mode::Pause);
        assert_eq!(back.windows, PeakConfig::default().windows);
    }
}
