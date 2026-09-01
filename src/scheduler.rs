#![allow(dead_code)]

use crate::adapters::satori::{LockedWriter, api};
use crate::event::Context;
use chrono::{DateTime, Datelike, Local, TimeZone, Weekday};
use rand::RngExt;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::task::AbortHandle;

/// 多群推送时，两个群之间的等待区间（秒）。
///
/// 取随机值而非固定值：固定间隔在多群、多任务并存时会形成整齐的节拍，
/// 既像机器人又容易撞上风控阈值；随机化后每轮的落点都不一样。
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    pub min_seconds: u64,
    pub max_seconds: u64,
}

impl Pace {
    pub fn new(min_seconds: u64, max_seconds: u64) -> Self {
        Self {
            min_seconds,
            max_seconds,
        }
    }

    /// 归一化到 [1, 600] 秒，并保证 min <= max（配置写反时不至于 panic）
    fn bounds(&self) -> (u64, u64) {
        let min = self.min_seconds.clamp(1, 600);
        let max = self.max_seconds.clamp(min, 600);
        (min, max)
    }

    /// 随机等待一段间隔
    pub async fn wait(&self) {
        tokio::time::sleep(Duration::from_secs(self.pick())).await;
    }

    fn pick(&self) -> u64 {
        let (min, max) = self.bounds();
        if min == max {
            return min;
        }
        rand::rng().random_range(min..=max)
    }
}

impl Default for Pace {
    fn default() -> Self {
        Self::new(20, 75)
    }
}

/// 周期性推送的触发条件（每天到点后，再判断今天是否命中）
#[derive(Clone, Copy)]
pub enum PushFrequency {
    /// 每日触发
    Daily,
    /// 每周指定星期触发
    Weekly(Weekday),
    /// 每月指定日触发 (1..=31)；当月没有该日则跳过
    Monthly(u32),
}

impl PushFrequency {
    fn matches(&self, now: DateTime<Local>) -> bool {
        match self {
            PushFrequency::Daily => true,
            PushFrequency::Weekly(wd) => now.weekday() == *wd,
            PushFrequency::Monthly(d) => now.day() == *d,
        }
    }

    fn describe(&self) -> String {
        match self {
            PushFrequency::Daily => "每日".to_string(),
            PushFrequency::Weekly(wd) => {
                let name = match wd {
                    Weekday::Mon => "周一",
                    Weekday::Tue => "周二",
                    Weekday::Wed => "周三",
                    Weekday::Thu => "周四",
                    Weekday::Fri => "周五",
                    Weekday::Sat => "周六",
                    Weekday::Sun => "周日",
                };
                format!("每{}", name)
            }
            PushFrequency::Monthly(d) => format!("每月{}日", d),
        }
    }
}

/// 全局定时任务管理器
pub struct Scheduler {
    tasks: Mutex<HashMap<u64, AbortHandle>>,
    next_id: AtomicU64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// 添加一个灵活调度任务
    pub fn add_schedule<C, F, Fut>(&self, mut next_run_calculator: C, mut task_gen: F) -> u64
    where
        C: FnMut(DateTime<Local>) -> Option<DateTime<Local>> + Send + 'static,
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // 首次计算执行时间
        let mut next_time = next_run_calculator(Local::now());

        let handle = tokio::spawn(async move {
            while let Some(target_time) = next_time {
                let now = Local::now();

                // 计算需要 sleep 多久
                if target_time > now {
                    let duration = (target_time - now)
                        .to_std()
                        .unwrap_or(Duration::from_millis(0));
                    tokio::time::sleep(duration).await;
                }

                // 执行任务
                task_gen().await;

                // 计算下一次
                next_time = next_run_calculator(Local::now());
            }
        });

        let abort_handle = handle.abort_handle();
        self.tasks.lock().unwrap().insert(id, abort_handle);
        id
    }

    /// 兼容旧接口：固定间隔执行
    pub fn add_interval<F, Fut>(&self, duration: Duration, task_gen: F) -> u64
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.add_schedule(
            move |now| Some(now + chrono::Duration::from_std(duration).unwrap()),
            task_gen,
        )
    }

    /// 辅助方法：每天特定时间执行 (HH:MM:SS)
    pub fn add_daily_at<F, Fut>(&self, hour: u32, minute: u32, second: u32, task_gen: F) -> u64
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.add_schedule(
            move |now| {
                let today = now.date_naive();
                // 构造今天的目标时间
                let target_today = today
                    .and_hms_opt(hour, minute, second)
                    .and_then(|t| Local.from_local_datetime(&t).single());

                if let Some(target) = target_today
                    && target > now
                {
                    return Some(target);
                }

                // 如果今天已经过了，或者是无效时间（如夏令时跳变），则定在明天
                let tomorrow = today.succ_opt()?;
                tomorrow
                    .and_hms_opt(hour, minute, second)
                    .and_then(|t| Local.from_local_datetime(&t).single())
            },
            task_gen,
        )
    }

    /// 通用工具：配置并调度周期性主动推送任务
    /// 包含：时间解析、频率过滤、群列表获取、黑白名单过滤、遍历执行
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_periodic_push<F, Fut>(
        &self,
        ctx: Context,
        writer: LockedWriter,
        plugin_name: &str,
        task_label: &str,
        time_str: String,
        frequency: PushFrequency,
        pace: Pace,
        task_logic: F,
    ) where
        F: Fn(Context, LockedWriter, i64) -> Fut + Send + Sync + 'static + Clone,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // 1. 解析时间
        let parts: Vec<&str> = time_str.split(':').collect();
        let (h, m, s) = if parts.len() >= 2 {
            (
                parts[0].parse().unwrap_or(23),
                parts[1].parse().unwrap_or(30),
                parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0),
            )
        } else {
            (23, 30, 0)
        };

        let log_target = format!("Plugin/{}", plugin_name);
        let (pace_min, pace_max) = pace.bounds();
        info!(
            target: log_target.as_str(),
            "已计划[{}]推送: {} {:02}:{:02}:{:02}（群间隔 {}—{} 秒随机）",
            task_label, frequency.describe(), h, m, s, pace_min, pace_max
        );

        // 2. 调度任务
        let plugin_name_owned = plugin_name.to_string();
        let task_label_owned = task_label.to_string();
        self.add_daily_at(h, m, s, move || {
            let ctx = ctx.clone();
            let writer = writer.clone();
            let task_logic = task_logic.clone();
            let p_name = plugin_name_owned.clone();
            let label = task_label_owned.clone();
            let freq = frequency;

            async move {
                let log_target = format!("Plugin/{}", p_name);

                // 3. 频率过滤：非目标日直接跳过
                if !freq.matches(Local::now()) {
                    return;
                }

                info!(target: log_target.as_str(), "开始执行[{}]推送...", label);

                // 4. 获取群列表
                let groups = match api::get_group_list(&ctx, writer.clone(), false).await {
                    Ok(g) => g,
                    Err(e) => {
                        error!(target: log_target.as_str(), "[{}] 获取群列表失败: {}", label, e);
                        return;
                    }
                };

                // 5. 准备过滤规则
                let (whitelist_mode, whitelist, blacklist) = {
                    let guard = ctx.config.read().unwrap();
                    (
                        guard.global_filter.enable_whitelist,
                        guard.global_filter.whitelist.clone(),
                        guard.global_filter.blacklist.clone(),
                    )
                };

                // 6. 过滤目标群
                let target_groups: Vec<i64> = groups
                    .into_iter()
                    .map(|g| g.group_id)
                    .filter(|gid| {
                        if whitelist_mode {
                            whitelist.contains(gid)
                        } else {
                            !blacklist.contains(gid)
                        }
                    })
                    .collect();

                if target_groups.is_empty() {
                    info!(target: log_target.as_str(), "[{}] 没有符合条件的群组，跳过推送。", label);
                    return;
                }

                // 7. 遍历执行：群与群之间留一段随机间隔，避免所有群同一秒收到推送
                let total = target_groups.len();
                for (idx, gid) in target_groups.into_iter().enumerate() {
                    let should_skip = {
                        let guard = ctx.config.read().unwrap();
                        if guard.global_filter.enable_whitelist {
                            !guard.global_filter.whitelist.contains(&gid)
                        } else {
                            guard.global_filter.blacklist.contains(&gid)
                        }
                    };
                    if should_skip {
                        continue;
                    }

                    task_logic(ctx.clone(), writer.clone(), gid).await;

                    // 间隔防风控：随机等待，最后一个群不必再等
                    if idx + 1 < total {
                        pace.wait().await;
                    }
                }
                info!(target: log_target.as_str(), "[{}] 推送任务完成。", label);
            }
        });
    }

    pub fn remove(&self, id: u64) {
        if let Some(handle) = self.tasks.lock().unwrap().remove(&id) {
            handle.abort();
        }
    }

    pub fn shutdown(&self) {
        info!("正在清理定时任务...");
        let mut tasks = self.tasks.lock().unwrap();
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_normalizes_reversed_or_extreme_bounds() {
        assert_eq!(Pace::new(30, 10).bounds(), (30, 30));
        assert_eq!(Pace::new(0, 0).bounds(), (1, 1));
        assert_eq!(Pace::new(5, 9999).bounds(), (5, 600));
    }

    #[test]
    fn pace_picks_inside_bounds_and_varies() {
        let pace = Pace::new(10, 40);
        let picks: Vec<u64> = (0..64).map(|_| pace.pick()).collect();
        assert!(picks.iter().all(|v| (10..=40).contains(v)));
        // 随机而非定值：64 次取样出现两个以上不同值的概率极高
        assert!(picks.iter().any(|v| *v != picks[0]), "群间隔应当是随机的");
    }
}
