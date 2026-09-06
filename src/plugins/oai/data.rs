use super::types::{Config, GeneratingState, MjCache};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;

const DEFAULT_MODEL: &str = "gpt-5.6-luna";
const LEGACY_DEFAULT_MODEL: &str = "gpt-4o";
const CURRENT_DEFAULTS_VERSION: u32 = 2;

/// `pi` 房间的人设。
///
/// 只写「是谁、什么风格」；运行环境、工具策略与排版要求由
/// [`super::agent::build_instructions`] 在每次请求时按当前时间和端点能力生成——
/// 那些内容写死在人设里会随时间过期，也没法随托管检索的可用性变化。
const PI_PERSONA: &str = "你是 pi，一个务实、直接的通用助手，回答简洁但不省略关键依据。";

/// 旧版 `pi` 人设；把运行细节写进了人设，现已由 harness 动态生成。
const LEGACY_PI_PERSONA: &str = "You are pi, a capable general assistant. In this public room you can use a full-permission shell and live web search. Use tools whenever they make the answer more accurate; never invent tool results. For web research, include the source URLs you relied on.";

// 全局单例管理器
pub static MANAGER: OnceLock<Arc<Manager>> = OnceLock::new();

pub struct Manager {
    pub config: RwLock<Config>,
    pub generating: RwLock<GeneratingState>,
    pub mj_cache: RwLock<MjCache>,
    pub mj_inflight: RwLock<HashSet<String>>,
    pub path: PathBuf,
    pub mj_cache_path: PathBuf,
    pub mj_images_dir: PathBuf,
}

impl Manager {
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("config.json");
        let mj_cache_path = dir.join("mj-cache.json");
        let mj_images_dir = dir.join("mj-images");
        // 同步加载一次配置 (初始化时使用)
        let default = Config {
            default_model: DEFAULT_MODEL.to_string(),
            default_prompt: "You are a helpful assistant.".to_string(),
            ..Default::default()
        };

        let mut config = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or(default),
                Err(_) => default,
            }
        } else {
            default
        };

        let mut config_dirty = false;
        // 将旧版默认值迁移到工具调用能力更完整的模型；只执行一次，不干预后续手动设置。
        if config.defaults_version < CURRENT_DEFAULTS_VERSION {
            if config.default_model.trim().is_empty()
                || config.default_model.eq_ignore_ascii_case(LEGACY_DEFAULT_MODEL)
            {
                config.default_model = DEFAULT_MODEL.to_string();
            }
            if let Some(pi) = config
                .agents
                .iter_mut()
                .find(|agent| agent.name.eq_ignore_ascii_case("pi"))
                && (pi.model.trim().is_empty()
                    || pi.model.eq_ignore_ascii_case(LEGACY_DEFAULT_MODEL))
            {
                pi.model = DEFAULT_MODEL.to_string();
            }
            // 人设里写死的运行说明已改由 harness 生成；只替换没被管理员改过的那份。
            if let Some(pi) = config
                .agents
                .iter_mut()
                .find(|agent| agent.name.eq_ignore_ascii_case("pi"))
                && pi.system_prompt.trim() == LEGACY_PI_PERSONA
            {
                pi.system_prompt = PI_PERSONA.to_string();
            }
            config.defaults_version = CURRENT_DEFAULTS_VERSION;
            config_dirty = true;
        }

        // 老配置只迁移一次；之后若管理员主动删除 `pi`，尊重这一选择。
        if !config.pi_room_initialized {
            if !config
                .agents
                .iter()
                .any(|agent| agent.name.eq_ignore_ascii_case("pi"))
            {
                let model = if config.default_model.trim().is_empty() {
                    DEFAULT_MODEL
                } else {
                    &config.default_model
                };
                config.agents.push(super::types::Agent::new(
                    "pi",
                    model,
                    PI_PERSONA,
                    "终端与联网工具助手",
                ));
            }
            config.pi_room_initialized = true;
            config_dirty = true;
        }
        if config_dirty && let Ok(serialized) = serde_json::to_string_pretty(&config) {
            let _ = std::fs::write(&path, serialized);
        }

        let mj_cache = std::fs::read_to_string(&mj_cache_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let _ = std::fs::create_dir_all(&mj_images_dir);

        Self {
            config: RwLock::new(config),
            generating: RwLock::new(GeneratingState::default()),
            mj_cache: RwLock::new(mj_cache),
            mj_inflight: RwLock::new(HashSet::new()),
            path,
            mj_cache_path,
            mj_images_dir,
        }
    }

    pub fn save(&self, cfg: &Config) {
        if let Ok(s) = serde_json::to_string_pretty(cfg) {
            // 使用 std::fs 写文件，虽然是阻塞操作，但保存配置频率不高
            let _ = std::fs::write(&self.path, s);
        }
    }

    pub fn save_mj_cache(&self, cache: &MjCache) {
        if let Ok(s) = serde_json::to_string_pretty(cache) {
            let _ = std::fs::write(&self.mj_cache_path, s);
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
        let base = base.trim_end_matches('/');
        let mut urls = vec![format!("{base}/models")];
        if !base.ends_with("/v1") {
            urls.push(format!("{base}/v1/models"));
        }
        let mut body = None;
        let mut last_error = String::new();
        for url in urls {
            match crate::http::client().get(&url).bearer_auth(&key).send().await {
                Ok(resp) if resp.status().is_success() => match resp.json().await {
                    Ok(value) => {
                        body = Some(value);
                        break;
                    }
                    Err(e) => last_error = format!("{url}: {e}"),
                },
                Ok(resp) => last_error = format!("{url}: HTTP {}", resp.status().as_u16()),
                Err(e) => last_error = format!("{url}: {e}"),
            }
        }
        let body: serde_json::Value =
            body.ok_or_else(|| anyhow::anyhow!("模型列表请求失败: {last_error}"))?;
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
        let mut final_models = if filtered.is_empty() {
            models
        } else {
            filtered
        };
        for model in super::mj::MJ_MODELS {
            if !final_models.iter().any(|m| m == model) {
                final_models.push((*model).to_string());
            }
        }

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
        if let Some(exact) = models.iter().find(|model| model.to_lowercase() == lower) {
            return Some(exact.clone());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_pi_room_once() {
        let unique = format!(
            "ayjx-oai-pi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();

        let manager = Manager::new(dir.clone());
        let serialized = std::fs::read_to_string(&manager.path).unwrap();
        let config: Config = serde_json::from_str(&serialized).unwrap();
        let pi = config.agents.iter().find(|agent| agent.name == "pi").unwrap();
        assert_eq!(pi.description, "终端与联网工具助手");
        assert_eq!(pi.model, DEFAULT_MODEL);
        assert!(pi.public_history.is_empty());
        assert_eq!(config.default_model, DEFAULT_MODEL);
        assert_eq!(config.defaults_version, CURRENT_DEFAULTS_VERSION);
        assert!(config.pi_room_initialized);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
