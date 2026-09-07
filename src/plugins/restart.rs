//! restart 插件:每日定时自动重启 + 内存阈值监控 + 手动重启指令
//!
//! 解决长时间运行导致缓存累积、云服务器内存被撑爆的问题。
//! 重启请求交给主循环：停止适配器和任务、关闭数据库与浏览器、保存配置，
//! Unix 下使用 exec 替换当前进程，保持前台终端及 PID。

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::match_word_command;
use crate::config::{AppConfig, build_config};
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use toml::Value;

// ================= 配置定义 =================

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RestartConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    /// 每日自动重启时间 (HH:MM，24 小时制)，默认凌晨 4 点(群聊低峰期)
    #[serde(default = "default_time")]
    time: String,
    /// 进程自身 RSS 内存阈值 (MB)，超过则提前重启；0 表示关闭内存监控(仅 Linux 支持读取)
    #[serde(default)]
    memory_threshold_mb: u64,
    /// 内存巡检间隔(分钟)
    #[serde(default = "default_check_interval")]
    memory_check_interval_minutes: u64,
    /// 是否开放 /restart 手动指令(默认关闭，防止群内成员误触发整机重启)
    #[serde(default)]
    allow_manual_restart: bool,
    /// 重启前等待秒数(等待通知消息刷新到 WebSocket)
    #[serde(default = "default_delay")]
    restart_delay_seconds: u64,
    /// 外部重启命令(如 "systemctl restart ayjx")；配置后优先使用，代替进程自我拉起
    #[serde(default)]
    restart_command: String,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: default_time(),
            memory_threshold_mb: 0,
            memory_check_interval_minutes: default_check_interval(),
            allow_manual_restart: false,
            restart_delay_seconds: default_delay(),
            restart_command: String::new(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_time() -> String {
    "04:00".to_string()
}
fn default_check_interval() -> u64 {
    5
}
fn default_delay() -> u64 {
    3
}

// ================= 全局状态 =================

static RESTART_REQUEST: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub async fn wait_request() {
    RESTART_REQUEST.notified().await;
}

/// 重启防抖标记:防止定时任务与内存监控同时触发导致双重重启
static RESTARTING: AtomicBool = AtomicBool::new(false);

// ================= 插件生命周期 =================

pub fn default_config() -> Value {
    build_config(RestartConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let cfg = get_config::<RestartConfig>(&ctx, "restart").unwrap_or_default();
        if !cfg.enabled {
            return Ok(());
        }

        // 1. 每日定时重启
        let (h, m, s) = parse_time(&cfg.time);
        let daily_ctx = ctx.clone();
        ctx.scheduler.add_daily_at(h, m, s, move || {
            let ctx = daily_ctx.clone();
            async move {
                info!(
                    target: "Plugin/Restart",
                    "每日定时重启触发，开始执行重启流程..."
                );
                do_restart(&ctx, "每日定时".to_string()).await;
            }
        });
        info!(
            target: "Plugin/Restart",
            "已计划每日 {:02}:{:02}:{:02} 自动重启（系统本地时区）",
            h,
            m,
            s
        );

        // 2. 内存阈值监控(仅 Linux 支持读取自身 RSS)
        if cfg.memory_threshold_mb > 0 {
            let threshold = cfg.memory_threshold_mb;
            let interval_secs = cfg.memory_check_interval_minutes.max(1) * 60;
            let mem_ctx = ctx.clone();
            ctx.scheduler
                .add_interval(Duration::from_secs(interval_secs), move || {
                    let ctx = mem_ctx.clone();
                    async move {
                        match current_rss_mb() {
                            Some(mb) if mb >= threshold => {
                                warn!(
                                    target: "Plugin/Restart",
                                    "内存占用 {}MB 达到阈值 {}MB，提前重启",
                                    mb,
                                    threshold
                                );
                                do_restart(&ctx, format!("内存超限 ({}MB >= {}MB)", mb, threshold))
                                    .await;
                            }
                            Some(mb) => {
                                debug!(
                                    target: "Plugin/Restart",
                                    "内存巡检: {}/{}MB",
                                    mb,
                                    threshold
                                );
                            }
                            None => {
                                debug!(
                                    target: "Plugin/Restart",
                                    "当前平台不支持读取内存占用，跳过巡检"
                                );
                            }
                        }
                    }
                });
            info!(
                target: "Plugin/Restart",
                "已开启内存监控: 阈值 {}MB，每 {} 分钟巡检一次",
                threshold,
                cfg.memory_check_interval_minutes
            );
        }

        Ok(())
    })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if let Some(_cmd) = match_word_command(&ctx, "restart") {
            if !crate::plugins::ctl::is_manager(&ctx) {
                let msg = ctx.as_message().unwrap();
                send_msg(
                    &ctx,
                    writer,
                    msg.group_id(),
                    Some(msg.user_id()),
                    Message::new().text(crate::plugins::ctl::DENIED),
                )
                .await?;
                return Ok(None);
            }
            let cfg = get_config::<RestartConfig>(&ctx, "restart").unwrap_or_default();

            // 未开放手动重启时给出提示
            if !cfg.allow_manual_restart {
                let msg = ctx.as_message().unwrap();
                let reply = Message::new()
                    .reply(msg.message_id())
                    .text("⚠️ 重启指令未开放，可用 /设置 restart allow_manual_restart true 开启。");
                let _ = send_msg(&ctx, writer, msg.group_id(), Some(msg.user_id()), reply).await;
                return Ok(None);
            }

            // 防抖:重启已在进行中则忽略
            if RESTARTING.load(Ordering::SeqCst) {
                return Ok(None);
            }

            let msg = ctx.as_message().unwrap();
            let group_id = msg.group_id();
            let user_id = msg.user_id();
            let message_id = msg.message_id();
            let delay = cfg.restart_delay_seconds;

            let reply = Message::new()
                .reply(message_id)
                .text(format!("⏳ 收到，{} 秒后重启，稍等片刻~", delay));
            if let Err(e) = send_msg(&ctx, writer.clone(), group_id, Some(user_id), reply).await {
                error!(target: "Plugin/Restart", "重启通知发送失败: {}", e);
            }

            // 延迟执行重启，确保回复消息已刷新到 WebSocket
            let restart_ctx = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                do_restart(&restart_ctx, "手动指令".to_string()).await;
            });

            return Ok(None);
        }

        Ok(Some(ctx))
    })
}

// ================= 重启核心逻辑 =================

/// 只提出请求，统一由主循环清理，避免定时任务中止自身或两个进程同时写配置。
async fn do_restart(ctx: &Context, reason: String) {
    if !get_config::<RestartConfig>(ctx, "restart").is_some_and(|cfg| cfg.enabled) {
        return;
    }
    if RESTARTING.swap(true, Ordering::SeqCst) {
        info!(
            target: "Plugin/Restart",
            "重启流程已在进行中，忽略本次触发 ({})",
            reason
        );
        return;
    }
    info!(
        target: "Plugin/Restart",
        "========== 开始重启 (原因: {}) ==========",
        reason
    );

    RESTART_REQUEST.notify_one();
}

/// 资源清理并保存成功后调用。exec 保留终端、PID、环境和单实例锁。
pub fn relaunch(config: &AppConfig) -> Result<(), PluginError> {
    let cfg = config
        .plugins
        .get("restart")
        .map(|v| RestartConfig::deserialize(v.clone()))
        .transpose()?
        .unwrap_or_default();
    if !cfg.restart_command.trim().is_empty() {
        return spawn_external(cfg.restart_command.trim());
    }
    let exe = std::env::current_exe()?;
    // A release build may have replaced the executable while this process ran.
    let executable = exe.to_string_lossy();
    let executable = executable.strip_suffix(" (deleted)").unwrap_or(&executable);
    let mut cmd = std::process::Command::new(executable);
    cmd.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        info!(target: "Plugin/Restart", "配置已保存，正在原地重启（保留前台终端与 PID）...");
        return Err(cmd.exec().into());
    }

    #[cfg(not(unix))]
    {
        let child = cmd.spawn()?;
        info!(target: "Plugin/Restart", "已启动新进程，PID: {}", child.id());
        Ok(())
    }
}

/// 通过外部命令重启(如 systemctl / supervisor / nssm 等进程管理器)
fn spawn_external(command: &str) -> Result<(), PluginError> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let child = std::process::Command::new("cmd")
            .args(["/C", command])
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        info!(
            target: "Plugin/Restart",
            "已执行外部重启命令: {} (pid: {:?})",
            command,
            child.id()
        );
    }
    #[cfg(not(windows))]
    {
        let child = std::process::Command::new("sh")
            .args(["-c", command])
            .spawn()?;
        info!(
            target: "Plugin/Restart",
            "已执行外部重启命令: {} (pid: {:?})",
            command,
            child.id()
        );
    }
    Ok(())
}

// ================= 工具函数 =================

/// 解析 HH:MM 或 HH:MM:SS，非法输入回退默认 04:00。
fn parse_time(s: &str) -> (u32, u32, u32) {
    use chrono::{NaiveTime, Timelike};
    NaiveTime::parse_from_str(s.trim(), "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s.trim(), "%H:%M"))
        .map(|t| (t.hour(), t.minute(), t.second()))
        .unwrap_or((4, 0, 0))
}

/// 读取当前进程 RSS 内存 (MB)。仅 Linux 提供 /proc/self/status，其余平台返回 None
fn current_rss_mb() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
                return Some(kb / 1024);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <RestartConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_accepts_minutes_and_seconds_and_falls_back() {
        assert_eq!(parse_time("04:00"), (4, 0, 0));
        assert_eq!(parse_time(" 23:59:58 "), (23, 59, 58));
        assert_eq!(parse_time("26:71"), (4, 0, 0));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn rss_monitor_reads_self_status_on_linux_and_termux() {
        assert!(current_rss_mb().is_some());
    }
}
