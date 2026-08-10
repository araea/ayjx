//! restart 插件:每日定时自动重启 + 内存阈值监控 + 手动重启指令
//!
//! 解决长时间运行导致缓存累积、云服务器内存被撑爆的问题。
//! 重启流程:先拉起新进程(失败则放弃、保证服务连续性) → 保存配置 → 关闭数据库 → 销毁浏览器 → 退出旧进程。

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
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

/// 原始启动参数(不含 argv[0])，自我重启时原样还原
static ORIGINAL_ARGS: OnceLock<Vec<String>> = OnceLock::new();

/// 重启防抖标记:防止定时任务与内存监控同时触发导致双重重启
static RESTARTING: AtomicBool = AtomicBool::new(false);

// ================= 插件生命周期 =================

pub fn default_config() -> Value {
    build_config(RestartConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        // 记录原始启动参数，供自我重启时还原
        let _ = ORIGINAL_ARGS.set(std::env::args().skip(1).collect());

        let cfg = get_config::<RestartConfig>(&ctx, "restart").unwrap_or_default();
        if !cfg.enabled {
            return Ok(());
        }

        // 1. 每日定时重启
        let (h, m) = parse_time(&cfg.time);
        let daily_ctx = ctx.clone();
        ctx.scheduler.add_daily_at(h, m, 0, move || {
            let ctx = daily_ctx.clone();
            async move {
                info!(
                    target: "Plugin/restart",
                    "每日定时重启触发，开始执行重启流程..."
                );
                do_restart(&ctx, "每日定时".to_string()).await;
            }
        });
        info!(
            target: "Plugin/restart",
            "已计划每日 {:02}:{:02} 自动重启",
            h,
            m
        );

        // 2. 内存阈值监控(仅 Linux 支持读取自身 RSS)
        if cfg.memory_threshold_mb > 0 {
            let threshold = cfg.memory_threshold_mb;
            let interval_secs = cfg.memory_check_interval_minutes.max(1) * 60;
            let mem_ctx = ctx.clone();
            ctx.scheduler.add_interval(Duration::from_secs(interval_secs), move || {
                let ctx = mem_ctx.clone();
                async move {
                    match current_rss_mb() {
                        Some(mb) if mb >= threshold => {
                            warn!(
                                target: "Plugin/restart",
                                "内存占用 {}MB 达到阈值 {}MB，提前重启",
                                mb,
                                threshold
                            );
                            do_restart(&ctx, format!("内存超限 ({}MB >= {}MB)", mb, threshold))
                                .await;
                        }
                        Some(mb) => {
                            debug!(
                                target: "Plugin/restart",
                                "内存巡检: {}/{}MB",
                                mb,
                                threshold
                            );
                        }
                        None => {
                            debug!(
                                target: "Plugin/restart",
                                "当前平台不支持读取内存占用，跳过巡检"
                            );
                        }
                    }
                }
            });
            info!(
                target: "Plugin/restart",
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
        if let Some(_cmd) = match_command(&ctx, "restart") {
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
                error!(target: "Plugin/restart", "重启通知发送失败: {}", e);
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

/// 执行完整重启流程:先拉起新进程 → 清理资源 → 退出旧进程
async fn do_restart(ctx: &Context, reason: String) {
    if RESTARTING.swap(true, Ordering::SeqCst) {
        info!(
            target: "Plugin/restart",
            "重启流程已在进行中，忽略本次触发 ({})",
            reason
        );
        return;
    }
    info!(
        target: "Plugin/restart",
        "========== 开始重启 (原因: {}) ==========",
        reason
    );

    let cfg = get_config::<RestartConfig>(ctx, "restart").unwrap_or_default();

    // 1. 先拉起新进程(失败则不退出，保证服务连续性)
    let spawned = if !cfg.restart_command.trim().is_empty() {
        spawn_external(cfg.restart_command.trim())
    } else {
        spawn_self()
    };
    if let Err(e) = spawned {
        error!(target: "Plugin/restart", "拉起新进程失败: {}，放弃重启", e);
        RESTARTING.store(false, Ordering::SeqCst);
        return;
    }
    info!(target: "Plugin/restart", "新进程已拉起，开始清理资源...");

    // 2. 保存配置(加锁避免与 update_config 并发写文件)
    {
        let _guard = ctx.config_save_lock.lock().await;
        let snapshot = {
            let guard = ctx.config.read().unwrap();
            guard.clone()
        };
        if let Err(e) = snapshot.save(&ctx.config_path).await {
            error!(target: "Plugin/restart", "重启前保存配置失败: {}", e);
        }
    }

    // 3. 关闭数据库连接(WAL 模式下安全落盘)
    // 注: ctx.db 为共享引用，clone 连接池句柄后 close，等价于关闭整个连接池
    if let Err(e) = ctx.db.clone().close().await {
        error!(target: "Plugin/restart", "关闭数据库失败: {}", e);
    }

    // 4. 销毁全局无头浏览器实例，释放其内存
    cdp_html_shot::Browser::shutdown_global().await;

    info!(target: "Plugin/restart", "资源清理完毕，旧进程退出。");
    std::process::exit(0);
}

/// 自我拉起:用当前可执行文件 + 原始启动参数启动新进程
fn spawn_self() -> Result<(), PluginError> {
    let exe = std::env::current_exe()?;
    let args = ORIGINAL_ARGS.get().cloned().unwrap_or_default();

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 创建独立进程组，父进程退出后新进程不受控制台关闭信号影响
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let child = cmd.spawn()?;
    info!(
        target: "Plugin/restart",
        "已自我拉起新进程: {:?} (pid: {:?})",
        exe,
        child.id()
    );
    Ok(())
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
            target: "Plugin/restart",
            "已执行外部重启命令: {} (pid: {:?})",
            command,
            child.id()
        );
    }
    #[cfg(not(windows))]
    {
        let child = std::process::Command::new("sh").args(["-c", command]).spawn()?;
        info!(
            target: "Plugin/restart",
            "已执行外部重启命令: {} (pid: {:?})",
            command,
            child.id()
        );
    }
    Ok(())
}

// ================= 工具函数 =================

/// 解析 "HH:MM" 或 "HH:MM:SS" 为 (时, 分)，非法输入回退默认 04:00
fn parse_time(s: &str) -> (u32, u32) {
    let parts: Vec<&str> = s.split(':').collect();
    let h = parts
        .first()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(4)
        .min(23);
    let m = parts
        .get(1)
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0)
        .min(59);
    (h, m)
}

/// 读取当前进程 RSS 内存 (MB)。仅 Linux 提供 /proc/self/status，其余平台返回 None
fn current_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
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
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
