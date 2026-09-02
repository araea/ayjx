use super::types::{Config, GeneratingState};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

// 全局单例管理器
pub static MANAGER: OnceLock<Arc<Manager>> = OnceLock::new();

pub struct Manager {
    pub config: RwLock<Config>,
    pub generating: RwLock<GeneratingState>,
    pub path: PathBuf,
}

impl Manager {
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("config.json");
        // 同步加载一次配置 (初始化时使用)
        let default = Config {
            default_model: "gpt-4o".to_string(),
            default_prompt: "You are a helpful assistant.".to_string(),
            ..Default::default()
        };

        let config = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or(default),
                Err(_) => default,
            }
        } else {
            default
        };

        Self {
            config: RwLock::new(config),
            generating: RwLock::new(GeneratingState::default()),
            path,
        }
    }

    pub fn save(&self, cfg: &Config) {
        if let Ok(s) = serde_json::to_string_pretty(cfg) {
            // 使用 std::fs 写文件，虽然是阻塞操作，但保存配置频率不高
            let _ = std::fs::write(&self.path, s);
        }
    }

    pub async fn fetch_models(&self) -> anyhow::Result<Vec<String>> {
        let (base, key) = {
            let c = self.config.read().await;
            (c.api_base.clone(), c.api_key.clone())
        };
        if base.is_empty() {
            return Err(anyhow::anyhow!("API未配置"));
        }

        // 自实现 GET {base}/models：DeepSeek 官方（及多数中转）返回的 model 对象
        // 字段不全（缺 created），async-openai 的强类型反序列化会失败，这里宽松解析只取 id。
        let url = format!("{}/models", base.trim_end_matches('/'));
        let resp = crate::http::client()
            .get(&url)
            .bearer_auth(&key)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "模型列表请求失败: HTTP {}",
                resp.status().as_u16()
            ));
        }

        let body: serde_json::Value = resp.json().await?;
        let mut models: Vec<String> = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        models.sort();
        models.dedup();

        let filtered = super::utils::filter_models(&models);
        let final_models = if filtered.is_empty() {
            models
        } else {
            filtered
        };

        {
            let mut c = self.config.write().await;
            c.models = final_models.clone();
            self.save(&c);
        }
        Ok(final_models)
    }

    pub fn resolve_model(&self, input: &str, models: &[String]) -> Option<String> {
        if input.is_empty() {
            return None;
        }
        if let Ok(i) = input.parse::<usize>()
            && i > 0
            && i <= models.len()
        {
            return Some(models[i - 1].clone());
        }
        let lower = input.to_lowercase();
        for m in models {
            if m.to_lowercase().contains(&lower) {
                return Some(m.clone());
            }
        }
        Some(input.to_string())
    }

    pub async fn agent_names(&self) -> Vec<String> {
        self.config
            .read()
            .await
            .agents
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }
}
