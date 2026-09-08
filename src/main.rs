mod adapters;
mod command;
mod config;
mod db;
mod event;
#[macro_use]
mod log;
mod http;
mod matcher;
mod message;
mod plugins;
mod scheduler;

use crate::config::AppConfig;
use crate::event::{BotStatus, Context, EventType};
use crate::matcher::Matcher;
use crate::scheduler::Scheduler;
use cdp_html_shot::Browser;
use futures_util::FutureExt;
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::fs;
use tokio::signal;
use tokio::sync::Mutex as AsyncMutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut console = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--console" => console = true,
            // 控制通道的客户端：连上正在运行的实例执行一条 ctl 命令后退出，
            // 不碰数据库、配置与浏览器。凭据只从环境变量取，不上命令行。
            "--ctl" => {
                let command = args.next().unwrap_or_default();
                return match plugins::ctl::bridge::client(&command) {
                    Ok(text) => {
                        println!("{text}");
                        Ok(())
                    }
                    Err(text) => {
                        println!("{text}");
                        std::process::exit(1);
                    }
                };
            }
            "--help" | "-h" => {
                println!(
                    "ayjx [--console] [--ctl <命令>]\n--console 临时启用前台控制台，不修改 config.toml；输入 /ctl 查看用法，Ctrl+C 停止。\n--ctl 向正在运行的实例发送一条 ctl 命令并打印回执，需要本轮 pi 房间对话签发的凭据。"
                );
                return Ok(());
            }
            _ => return Err(format!("未知参数：{arg}；使用 --help 查看用法").into()),
        }
    }
    let config_path = "config.toml";

    let db = db::init().await.expect("数据库初始化失败");

    // 加载或创建基础配置
    let mut app_config = if Path::new(config_path).exists() {
        let content = fs::read_to_string(config_path).await?;
        match toml::from_str::<AppConfig>(&content) {
            Ok(cfg) => cfg,
            Err(e) => {
                // 如果文件存在但内容为空，使用默认配置
                if content.trim().is_empty() {
                    warn!("配置文件为空，将使用默认配置并重新生成。");
                    AppConfig::default()
                } else {
                    // 如果解析失败（如类型错误），直接报错退出，防止覆盖源文件
                    error!("配置文件 [{}] 解析失败: {}", config_path, e);
                    error!(
                        "请检查配置文件格式是否正确（例如字段类型是否匹配）。程序已停止以保护配置不被覆盖。"
                    );
                    // 显式指定错误类型，帮助编译器推断 main 函数的返回类型
                    let err: Box<dyn std::error::Error + Send + Sync> = Box::new(e);
                    return Err(err);
                }
            }
        }
    } else {
        // 文件不存在，使用默认配置
        AppConfig::default()
    };

    // 动态合并插件默认配置
    let registered_plugins = plugins::get_plugins();
    let mut config_dirty = false;

    // 清理无效配置：只保留注册过的插件配置
    let valid_plugin_names: HashSet<&str> = registered_plugins.iter().map(|p| p.name).collect();
    let unknown_keys: Vec<String> = app_config
        .plugins
        .keys()
        .filter(|k| !valid_plugin_names.contains(k.as_str()))
        .cloned()
        .collect();

    for key in unknown_keys {
        info!("清理无效配置项: [{}]", key);
        app_config.plugins.remove(&key);
        config_dirty = true;
    }

    for plugin in registered_plugins {
        let default_config = (plugin.default_config)();

        match app_config.plugins.get_mut(plugin.name) {
            Some(existing_config) => {
                // 如果配置已存在，尝试合并默认配置中的新字段
                if let toml::Value::Table(existing_table) = existing_config
                    && let toml::Value::Table(default_table) = default_config
                {
                    for (key, value) in default_table {
                        if !existing_table.contains_key(&key) {
                            info!("插件 [{}] 配置补全: 新增字段 '{}'", plugin.name, key);
                            existing_table.insert(key, value);
                            config_dirty = true;
                        }
                    }
                }
            }
            None => {
                info!("检测到新插件 [{}]，写入默认配置...", plugin.name);
                app_config
                    .plugins
                    .insert(plugin.name.to_string(), default_config);
                config_dirty = true;
            }
        }
    }

    if config_dirty || !Path::new(config_path).exists() {
        app_config.save(config_path).await?;
        if config_dirty {
            info!("配置文件已更新。");
        }
    }

    // The screenshot dependency panics when Chromium is absent. Browser availability
    // must not prevent text commands, recording and control from starting.
    let browser_init = async {
        match app_config.browser_path.as_deref().filter(|p| !p.is_empty()) {
            Some(path) => {
                let _ = Browser::instance_with_path(path).await;
            }
            None => {
                let _ = Browser::instance().await;
            }
        }
    };
    if AssertUnwindSafe(browser_init).catch_unwind().await.is_err() {
        warn!("浏览器不可用，继续启动；帮助可退回文字，截图功能需安装 Chrome/Chromium。");
    }

    // 构建运行时组件
    let shared_config = Arc::new(RwLock::new(app_config.clone()));
    // 初始化调度器
    let scheduler = Arc::new(Scheduler::new());
    // 初始化文件写入锁
    let save_lock = Arc::new(AsyncMutex::new(()));

    // === 触发插件初始化钩子 (生命周期: init) ===
    let shared_config_path: Arc<str> = Arc::from(config_path);
    let system_bot: Arc<BotStatus> = Arc::new(BotStatus {
        adapter: "system".to_string(),
        platform: "internal".to_string(),
        login_user: Default::default(),
    });
    let init_ctx = Context {
        event: EventType::Init,
        config: shared_config.clone(),
        config_save_lock: save_lock.clone(),
        db: db.clone(),
        scheduler: scheduler.clone(),
        matcher: Arc::new(Matcher::new()),
        config_path: shared_config_path.clone(),
        bot: system_bot.clone(),
    };
    plugins::do_init(init_ctx).await?;
    // ==========================================

    // 启动 Bots
    let mut active_bots = 0;
    let mut bot_configs = app_config.bots;
    if console {
        if let Some(bot) = bot_configs.iter_mut().find(|b| b.protocol == "console") {
            bot.enabled = true;
        } else {
            bot_configs.push(config::BotConfig {
                enabled: true,
                protocol: "console".into(),
                url: None,
                access_token: None,
            });
        }
    }
    let mut adapter_tasks = Vec::new();
    for bot_conf in bot_configs {
        // 1. 检查是否启用
        if !bot_conf.enabled {
            if bot_conf.protocol == "satori" {
                info!("Bot (Satori) 已禁用。若需启用请在配置文件中设置 enabled = true。");
            }
            continue;
        }

        // Satori 实现端地址为必填；token 可留空（取决于实现端配置）
        if bot_conf.protocol == "satori" {
            match &bot_conf.url {
                Some(u) if !u.is_empty() => u,
                _ => {
                    error!("Bot 配置错误: Satori 协议必须指定 url");
                    continue;
                }
            };
        }

        let adapter = if let Some(a) = adapters::find_adapter(&bot_conf.protocol) {
            a
        } else {
            error!("Bot 配置了未知的协议 '{}'，跳过。", bot_conf.protocol);
            continue;
        };

        active_bots += 1;
        let bot_shared_cfg = shared_config.clone();
        let bot_scheduler = scheduler.clone();
        let bot_save_lock = save_lock.clone();
        let bot_config_path = shared_config_path.clone();
        let bot_db = db.clone();
        let handler = adapter.handler;
        let protocol_name = bot_conf.protocol.clone();

        let bot_url = bot_conf
            .url
            .clone()
            .unwrap_or_else(|| "Internal".to_string());

        adapter_tasks.push(tokio::spawn(async move {
            info!("启动适配器 [{}] -> {}", protocol_name, bot_url);
            handler(
                bot_conf,
                bot_shared_cfg,
                bot_db,
                bot_scheduler,
                bot_save_lock,
                bot_config_path,
            )
            .await;
        }));
    }

    info!("激活 Bot 数量: {}。按 Ctrl+C 退出。", active_bots);

    // 等待退出信号 (优雅关闭): 同时监听 SIGINT (Ctrl+C) 与 SIGTERM (kill/systemd stop)
    #[cfg(unix)]
    let restarting = {
        use tokio::signal::unix::{SignalKind, signal as unix_signal};
        let mut sigterm = unix_signal(SignalKind::terminate())?;
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("收到退出信号 (Ctrl+C)，正在清理资源...");
                false
            }
            _ = sigterm.recv() => {
                info!("收到退出信号 (SIGTERM)，正在清理资源...");
                false
            }
            _ = plugins::restart::wait_request() => true,
        }
    };
    #[cfg(not(unix))]
    let restarting = tokio::select! {
        _ = signal::ctrl_c() => false,
        _ = plugins::restart::wait_request() => true,
    };

    // 执行清理工作 (带超时保护，避免浏览器销毁等操作卡死导致进程挂起)
    let cleanup = async {
        for task in adapter_tasks {
            task.abort();
            let _ = task.await;
        }
        scheduler.shutdown();
        let _ = db.close().await;
        cdp_html_shot::Browser::shutdown_global().await;
    };
    if tokio::time::timeout(std::time::Duration::from_secs(25), cleanup)
        .await
        .is_err()
    {
        error!("清理超时 (25s)，强制退出。");
        std::process::exit(1);
    }

    // 退出前强制再保存一次配置，确保万无一失
    let _save_guard = save_lock.lock().await;
    let config_snapshot = if let Ok(guard) = shared_config.read() {
        Some(guard.clone())
    } else {
        error!("无法获取配置锁，跳过保存。");
        None
    };

    if let Some(ref cfg) = config_snapshot {
        if let Err(e) = cfg.save(config_path).await {
            error!("退出前保存配置失败: {}", e);
            if restarting {
                return Err(e);
            }
        } else {
            info!("配置已保存。");
        }
    }

    drop(_save_guard);
    if restarting {
        let cfg = config_snapshot.ok_or("无法读取配置，重启已取消")?;
        plugins::restart::relaunch(&cfg)?;
    }

    info!("Bye!");
    Ok(())
}
