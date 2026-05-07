use crate::config::{AppConfig, BotConfig};
use crate::scheduler::Scheduler;
use futures_util::future::BoxFuture;
use sea_orm::DatabaseConnection;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::Mutex as AsyncMutex;

pub mod console;
pub mod onebot;

/// 适配器处理函数签名
pub type AdapterHandler = fn(
    BotConfig,
    Arc<RwLock<AppConfig>>,
    DatabaseConnection,
    Arc<Scheduler>,
    Arc<AsyncMutex<()>>,
    Arc<str>,
) -> BoxFuture<'static, ()>;

/// 适配器定义
pub struct Adapter {
    /// 协议名称 (如 "onebot")，在配置文件中通过 protocol 字段指定
    pub protocol: &'static str,
    /// 启动处理函数
    pub handler: AdapterHandler,
}

static ADAPTERS: OnceLock<Vec<Adapter>> = OnceLock::new();

/// 获取所有注册的适配器
pub fn get_adapters() -> &'static [Adapter] {
    ADAPTERS.get_or_init(|| {
        vec![
            // 注册 OneBot 适配器
            Adapter {
                protocol: "onebot",
                handler: onebot::entry,
            },
            // 注册控制台适配器 (用于测试)
            Adapter {
                protocol: "console",
                handler: console::entry,
            },
        ]
    })
}

/// 根据协议名称查找适配器
pub fn find_adapter(protocol: &str) -> Option<&'static Adapter> {
    get_adapters().iter().find(|a| a.protocol == protocol)
}
