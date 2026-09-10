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
        // Write beside the destination and rename only after syncing a complete file.
        use tokio::io::AsyncWriteExt;
        let temporary = format!(
            "{}.tmp-{}-{}",
            path,
            std::process::id(),
            rand::random::<u64>()
        );
        let result = async {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await?;
            file.write_all(toml_string.as_bytes()).await?;
            file.sync_all().await?;
            fs::rename(&temporary, path).await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result?;
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
/// 把默认配置里缺的字段补进现有配置，逐层深入；只补空缺，从不覆盖已有取值。
///
/// 升级带来的新字段要能被 `/ctl` 看到和改到，而 `/ctl` 的路径解析走不进一个不存在的
/// 键。只补最外层的话，`[oai.ambient]` 这种嵌套表里新增的开关就永远停在
/// 「配置路径不存在」——运行时靠 serde 默认值照常工作，管理员却一辈子改不了它。
///
/// 返回是否真的补了东西，调用方据此决定要不要落盘。
pub fn fill_missing(existing: &mut toml::value::Table, defaults: Value, label: &str) -> bool {
    let Value::Table(defaults) = defaults else {
        return false;
    };
    let mut changed = false;
    for (key, value) in defaults {
        match existing.get_mut(&key) {
            None => {
                crate::info!("配置补全：{label}.{key}");
                existing.insert(key, value);
                changed = true;
            }
            // 两边都是表才往下走；一边是表一边是标量，说明管理员改过类型，别动它。
            Some(Value::Table(nested)) => {
                changed |= fill_missing(nested, value, &format!("{label}.{key}"));
            }
            Some(_) => {}
        }
    }
    changed
}

pub fn build_config<T: Serialize>(data: T) -> Value {
    let mut val = Value::try_from(data).unwrap_or(Value::Table(Default::default()));
    if let Value::Table(ref mut map) = val
        && !map.contains_key("enabled")
    {
        map.insert("enabled".to_string(), Value::Boolean(true));
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_reach_fields_nested_inside_existing_tables() {
        let mut stored = match toml::from_str::<Value>(
            "enabled = true\n[ambient]\nscore_threshold = 60\n[ambient.peak]\nmode = \"pause\"",
        )
        .unwrap()
        {
            Value::Table(table) => table,
            _ => unreachable!(),
        };
        let defaults: Value = toml::from_str(
            "enabled = false\ntimeout = 30\n[ambient]\nscore_threshold = 45\nmood_enabled = true\n[ambient.peak]\nmode = \"sleep\"\nwindows = [\"09:00-12:00\"]",
        )
        .unwrap();

        assert!(fill_missing(&mut stored, defaults.clone(), "test"));
        // 管理员的取值一个都没被动过，深到第三层也一样。
        assert_eq!(stored["enabled"].as_bool(), Some(true));
        assert_eq!(stored["ambient"]["score_threshold"].as_integer(), Some(60));
        assert_eq!(stored["ambient"]["peak"]["mode"].as_str(), Some("pause"));
        // 新字段补齐了，每一层都补。
        assert_eq!(stored["timeout"].as_integer(), Some(30));
        assert_eq!(stored["ambient"]["mood_enabled"].as_bool(), Some(true));
        assert!(stored["ambient"]["peak"]["windows"].is_array());
        // 补过一次之后就没有改动了，不该每次启动都重写配置文件。
        assert!(!fill_missing(&mut stored, defaults, "test"));
    }

    #[test]
    fn a_field_whose_type_was_changed_by_hand_is_left_alone() {
        let mut stored = match toml::from_str::<Value>("channel = \"all\"").unwrap() {
            Value::Table(table) => table,
            _ => unreachable!(),
        };
        let defaults: Value = toml::from_str("[channel]\nwhite = []").unwrap();
        assert!(!fill_missing(&mut stored, defaults, "test"));
        assert_eq!(stored["channel"].as_str(), Some("all"));
    }
}
