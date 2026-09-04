use crate::config::build_config;
use serde::{Deserialize, Serialize};
use toml::Value;

/// 词云配置：容器级 `#[serde(default)]` 让缺省字段全部回落到 `Default`，单一事实来源。
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct WordCloudConfig {
    pub enabled: bool,
    pub limit: usize,
    pub width: u32,
    pub height: u32,
    pub font_path: Option<String>,
    pub font_family: Option<String>,
    pub max_msg: usize,
}

impl Default for WordCloudConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: 50,
            width: 800,
            height: 600,
            font_path: None,
            font_family: None,
            max_msg: 50000,
        }
    }
}

pub fn default_config() -> Value {
    build_config(WordCloudConfig::default())
}
