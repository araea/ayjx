use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::fs;
use toml::Value;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppConfig {
    // 全局指令前缀（支持多个，如 ["/", "#"]）
    #[serde(default = "default_prefix")]
    pub command_prefix: Vec<String>,

    // 全局浏览器路径配置 (默认为空，即自动查找)
    #[serde(default)]
    pub browser_path: Option<String>,

    // 全局频道过滤配置
    #[serde(default)]
    pub global_filter: GlobalFilterConfig,

    // Bot 连接配置
    #[serde(default = "default_bots")]
    pub bots: Vec<BotConfig>,

    // 插件配置
    #[serde(flatten)]
    pub plugins: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct GlobalFilterConfig {
    #[serde(default)]
    pub enable_blacklist: bool,
    #[serde(default)]
    pub blacklist: Vec<i64>,

    #[serde(default)]
    pub enable_whitelist: bool,
    #[serde(default)]
    pub whitelist: Vec<i64>,
}

impl AppConfig {
    pub async fn save(&self, path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let toml_string = toml::to_string_pretty(self)?;
        fs::write(path, toml_string).await?;
        Ok(())
    }
}

fn default_prefix() -> Vec<String> {
    vec!["/".to_string()]
}

fn default_bots() -> Vec<BotConfig> {
    vec![
        // 控制台适配器：保持简洁，仅需启用
        BotConfig {
            enabled: false,
            protocol: "console".to_string(),
            url: None,
            access_token: None,
        },
        // Satori 适配器：默认连接本机 satori-qq
        BotConfig {
            enabled: true,
            protocol: "satori".to_string(),
            url: Some("http://127.0.0.1:3001".to_string()),
            access_token: None,
        },
    ]
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BotConfig {
    // 是否启用此 Bot
    #[serde(default = "default_true")]
    pub enabled: bool,

    // 协议类型 (例如 "satori")
    #[serde(default = "default_protocol")]
    pub protocol: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_protocol() -> String {
    "satori".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            command_prefix: default_prefix(),
            browser_path: None,
            global_filter: GlobalFilterConfig::default(),
            bots: default_bots(),
            plugins: HashMap::new(),
        }
    }
}

/// 辅助函数：构建默认配置 Value，并确保包含 enabled 字段
pub fn build_config<T: Serialize>(data: T) -> Value {
    let mut val = Value::try_from(data).unwrap_or(Value::Table(Default::default()));
    if let Value::Table(ref mut map) = val
        && !map.contains_key("enabled")
    {
        map.insert("enabled".to_string(), Value::Boolean(true));
    }
    val
}
