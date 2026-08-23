use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PluginConfig {
    #[serde(default)]
    pub at_user: bool,
    #[serde(default = "default_true")]
    pub quote_user: bool,
    #[serde(default)]
    pub direct_guess: bool,
    #[serde(default = "default_history_display")]
    pub history_display: usize,
    #[serde(default = "default_rank_display")]
    pub rank_display: usize,
    /// 盘面、揭晓、排行榜、说明是否排版成卡片图；关掉则一律发文本。
    /// 「不在词库中」这类即时纠错任何时候都走文本，不受此项影响。
    #[serde(default = "default_true")]
    pub image_enabled: bool,
    /// 出图的设备像素比：版心宽度不变，倍数越高越清晰也越大（1—4）
    #[serde(default = "default_image_scale")]
    pub image_scale: f64,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            at_user: false,
            quote_user: true,
            direct_guess: false,
            history_display: 10,
            rank_display: 10,
            image_enabled: default_true(),
            image_scale: default_image_scale(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_history_display() -> usize {
    10
}
fn default_rank_display() -> usize {
    10
}
fn default_image_scale() -> f64 {
    3.0
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CiYiConfig {
    #[serde(default)]
    pub plugin: PluginConfig,
}
