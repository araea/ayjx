use crate::config::build_config;
use serde::{Deserialize, Serialize};
use toml::Value;

#[derive(Serialize, Deserialize, Clone)]
pub struct WordCloudConfig {
    pub enabled: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub font_path: Option<String>,
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default = "default_max_msg")]
    pub max_msg: usize,
}

fn default_limit() -> usize {
    50
}

fn default_width() -> u32 {
    800
}

fn default_height() -> u32 {
    600
}

fn default_max_msg() -> usize {
    50000
}

pub fn default_config() -> Value {
    build_config(WordCloudConfig {
        enabled: true,
        limit: 50,
        width: 800,
        height: 600,
        font_path: None,
        font_family: None,
        max_msg: 50000,
    })
}
