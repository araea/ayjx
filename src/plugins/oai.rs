use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::{PluginError, get_data_dir};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};

use std::collections::HashMap;
use std::sync::Arc;
use toml::Value;

pub(crate) mod ambient;
pub mod data;
pub mod images;
pub mod logic;
pub mod mj;
pub mod parser;
mod pi_agent;
pub mod render;
pub mod types;
pub mod utils;

use data::MANAGER;

/// 一个模型供应商的接入点与密钥。
///
/// 房间模型、判定模型写成 `供应商/模型` 时，就按这里的名字取接口；
/// 不带前缀的仍走 `oai` 自己那份默认配置（历史上就是 apilio 中转站）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(crate) struct ProviderConfig {
    /// OpenAI 兼容接口基址，例如 `https://api.deepseek.com/v1`。
    pub api_base: String,
    /// 该供应商的 APIKEY。
    pub api_key: String,
}

/// 解析一次请求要用的接口地址与密钥。
///
/// - `provider` 为空：用默认接口（`oai` 数据配置，保持既有房间行为不变）；
/// - 指定名字且 `[oai.providers]` 里有：用该供应商的配置；
/// - 指定 `apilio` 但没单独配置：回退到默认接口（它就是历史默认，避免重复填写）；
/// - 其他未知名字：返回 `None`，宁可报错也不悄悄打到别的站点。
pub(crate) fn resolve_endpoint(
    providers: &HashMap<String, ProviderConfig>,
    default_base: &str,
    default_key: &str,
    provider: Option<&str>,
) -> Option<(String, String)> {
    let name = provider.map(str::trim).filter(|name| !name.is_empty());
    match name {
        None => Some((default_base.to_string(), default_key.to_string())),
        Some(name) => {
            if let Some(found) = providers.get(name) {
                return Some((found.api_base.clone(), found.api_key.clone()));
            }
            name.eq_ignore_ascii_case("apilio")
                .then(|| (default_base.to_string(), default_key.to_string()))
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct OaiConfig {
    enabled: bool,
    /// 本机 Pi CLI 可执行文件；模型与工具沿用 Pi 配置。
    pi_command: String,
    /// 单次回复的总时间预算。
    request_timeout_seconds: u64,
    /// pi 的事件流静默多少秒算卡死（卡住且尚未出正文时自动重来一次）；置 0 关闭。
    /// pi 自身不给模型请求设超时，中转站抽风时它会一直等到总预算耗尽。
    pi_stall_seconds: u64,
    /// 短回复直接以文本发送而不渲染图片的字符上限；置 0 表示始终渲染图片。
    /// 一句话的答复走文本既快又便于复制。
    plain_text_max_chars: usize,
    /// 在回复卡片页脚展示模型、耗时与工具调用轨迹。
    show_trace_footer: bool,
    /// 模型列表过滤：中转站返回的上千个 id 里只留下当下值得用的那些。
    /// 站点上新或下架时改这里即可，`/%` 会按新规则重新拉取。
    pub(crate) model_filter: utils::ModelFilterConfig,
    /// 走 `/v1/images/generations` 的图像模型关键字（不区分大小写、子串匹配）。
    /// 命中的房间把提示词交给专用图像接口，其余房间仍走聊天补全。
    pub(crate) image_models: Vec<String>,
    /// 可选供应商表：模型写 `供应商/模型` 时按名字取这里的接口与密钥。
    /// 不配也不影响既有房间——不带前缀的仍走 `oai` 默认接口。
    pub(crate) providers: HashMap<String, ProviderConfig>,
    /// 群聊搭话：以固定人格作为群成员之一存在，绝大多数时候沉默。
    ambient: ambient::AmbientConfig,
}

impl Default for OaiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pi_command: "pi".to_string(),
            request_timeout_seconds: 300,
            pi_stall_seconds: 90,
            plain_text_max_chars: 120,
            show_trace_footer: true,
            model_filter: utils::ModelFilterConfig::default(),
            image_models: images::DEFAULT_IMAGE_MODELS
                .iter()
                .map(|keyword| (*keyword).to_string())
                .collect(),
            providers: HashMap::new(),
            ambient: ambient::AmbientConfig::default(),
        }
    }
}

impl OaiConfig {
    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_seconds.clamp(30, 1_800))
    }

    /// pi 静默多久算卡死；`None` 表示不看。
    pub(crate) fn pi_stall(&self) -> Option<std::time::Duration> {
        (self.pi_stall_seconds > 0)
            .then(|| std::time::Duration::from_secs(self.pi_stall_seconds.max(20)))
    }

    pub(crate) fn plain_text_max_chars(&self) -> usize {
        self.plain_text_max_chars
    }

    pub(crate) fn show_trace_footer(&self) -> bool {
        self.show_trace_footer
    }
}

pub fn default_config() -> Value {
    build_config(OaiConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let dir = get_data_dir("oai").await?;
        if let Err(error) = ambient::init(&dir).await {
            warn!(target: "Plugin/OAI", "群聊搭话资源初始化失败: {}", error);
        }
        let mgr = Arc::new(data::Manager::new(dir));

        // 尝试预加载模型列表
        let filter = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").model_filter;
        let mgr_clone = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr_clone.fetch_models(&filter).await {
                warn!(target: "Plugin/OAI", "初始化获取模型列表失败: {}", e);
            } else {
                info!(target: "Plugin/OAI", "初始化获取模型列表成功");
            }
        });

        if MANAGER.set(mgr).is_err() {
            warn!(target: "Plugin/OAI", "Manager 已经被初始化");
        }
        Ok(())
    })
}

// 提取纯文本内容，自动忽略头部的 At 和 Reply 消息段
fn extract_clean_text(ctx: &Context) -> Option<String> {
    let event = match &ctx.event {
        crate::event::EventType::Satori(e) => e,
        _ => return None,
    };

    if event.get_str("post_type")? != "message" {
        return None;
    }

    let arr = event.get_array("message")?;
    let mut text_acc = String::new();
    let mut found_start = false;

    for seg in arr {
        let type_ = seg.get_str("type")?;

        if !found_start {
            // 跳过头部的 at 和 reply
            if type_ == "at" || type_ == "reply" {
                continue;
            }
            // 如果是文本，检查是否为空白
            if type_ == "text" {
                let data = seg.get("data")?;
                let t = data.get_str("text").unwrap_or("");
                let trimmed = t.trim_start();
                if trimmed.is_empty() {
                    continue;
                }
                // 找到有效文本起点
                found_start = true;
                text_acc.push_str(trimmed);
            } else {
                // 遇到非文本（如图片），视为内容开始，停止跳过
                found_start = true;
            }
        } else if type_ == "text" {
            let t = seg.get("data")?.get_str("text").unwrap_or("");
            text_acc.push_str(t);
        }
    }

    if text_acc.is_empty() {
        None
    } else {
        Some(text_acc)
    }
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        // 确保 Manager 已初始化
        let mgr = match MANAGER.get() {
            Some(m) => m,
            None => {
                error!(target: "Plugin/OAI", "插件尚未初始化");
                return Ok(Some(ctx));
            }
        };

        // MJ 的放大交互只依赖被引用的机器人消息，不要求再次写房间名。
        // 因此必须先于普通文本/指令解析处理，也能兼容只发送数字的回复。
        if mj::try_handle_upscale_reply(&ctx, &writer, mgr).await {
            return Ok(None);
        }

        // 获取纯文本内容。没有文字的消息（纯图片、纯表情）不可能是指令，
        // 但仍是群聊上下文的一部分，要让搭话观察者看到。
        let raw_text = match extract_clean_text(&ctx) {
            Some(t) => t,
            None => {
                ambient::observe(&ctx, &writer, mgr).await;
                return Ok(Some(ctx));
            }
        };

        // 1. 全局指令解析
        if let Some(cmd) = parser::parse_global(&raw_text, &crate::command::get_prefixes(&ctx)) {
            logic::execute(cmd, String::new(), vec![], &ctx, &writer, mgr).await;
            return Ok(None); // 指令被消费，不再传递
        }

        // 2. 创建指令解析
        if let Some((name, desc, model, prompt)) = parser::parse_create(&raw_text) {
            logic::handle_create(&name, &desc, &model, &prompt, &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 3. 删除指令解析
        let agents = mgr.agent_names().await;
        if let Some(name) = parser::parse_delete_agent(&raw_text, &agents) {
            let cmd = parser::Command::new(&name, parser::Action::Delete);
            logic::execute(cmd, String::new(), vec![], &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 4. 智能体指令/对话解析
        if let Some(cmd) = parser::parse_agent_cmd(&raw_text, &agents) {
            let (quote, imgs) = utils::get_full_content(&ctx, &writer, Some(&cmd.agent)).await;

            // 拼接提示词：引用 + 用户输入参数
            let prompt = if matches!(
                cmd.action,
                parser::Action::Chat | parser::Action::Regenerate
            ) {
                format!("{}{}", quote, cmd.args).trim().to_string()
            } else {
                cmd.args.clone()
            };

            logic::execute(cmd, prompt, imgs, &ctx, &writer, mgr).await;
            return Ok(None);
        }

        // 不属于任何指令的普通群聊：交给搭话观察者，事件继续向后传递。
        ambient::observe(&ctx, &writer, mgr).await;

        Ok(Some(ctx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_route_by_prefix_and_apilio_tracks_the_default_endpoint() {
        let mut providers = HashMap::new();
        providers.insert(
            "deepseek".to_string(),
            ProviderConfig {
                api_base: "https://api.deepseek.com/v1".into(),
                api_key: "sk-deepseek".into(),
            },
        );
        let resolve = |name: Option<&str>| {
            resolve_endpoint(&providers, "https://api.apilio.ai/v1", "sk-apilio", name)
        };
        // 不带前缀：默认接口，既有房间行为不变。
        assert_eq!(
            resolve(None),
            Some(("https://api.apilio.ai/v1".into(), "sk-apilio".into()))
        );
        // 新供应商按名字取自己的接口。
        assert_eq!(
            resolve(Some("deepseek")),
            Some(("https://api.deepseek.com/v1".into(), "sk-deepseek".into()))
        );
        // apilio 不必重复配置，始终回退到默认接口，避免与 `oai <url> <key>` 脱节。
        assert_eq!(
            resolve(Some("apilio")),
            Some(("https://api.apilio.ai/v1".into(), "sk-apilio".into()))
        );
        // 未知供应商宁可报错，也不悄悄打到别的站点。
        assert_eq!(resolve(Some("unknown")), None);
        assert_eq!(resolve(Some("  ")), Some(("https://api.apilio.ai/v1".into(), "sk-apilio".into())));
    }

    #[test]
    fn pi_command_defaults_and_legacy_config_remain_loadable() {
        let config: OaiConfig =
            toml::from_str("harness_rooms = ['pi']\nshell_timeout_seconds = 300").unwrap();
        assert_eq!(config.pi_command, "pi");
        let config: OaiConfig = toml::from_str("pi_command = '/custom/pi'").unwrap();
        assert_eq!(config.pi_command, "/custom/pi");
    }

    #[test]
    fn provider_table_is_optional_and_parses_when_present() {
        // 旧配置没有 providers 也能照常加载。
        let legacy: OaiConfig = toml::from_str("pi_command = 'pi'").unwrap();
        assert!(legacy.providers.is_empty());
        let config: OaiConfig = toml::from_str(
            "[providers.deepseek]\napi_base = 'https://api.deepseek.com/v1'\napi_key = 'sk-x'",
        )
        .unwrap();
        assert_eq!(
            config.providers.get("deepseek").map(|p| p.api_base.as_str()),
            Some("https://api.deepseek.com/v1")
        );
    }
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <OaiConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}
