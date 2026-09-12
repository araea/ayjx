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

pub mod data;
pub mod images;
pub(crate) mod llm;
pub mod logic;
pub mod mj;
pub mod music;
pub mod parser;
pub(crate) mod agent;
pub(crate) mod presets;
pub mod render;
pub(crate) mod search;
pub mod types;
pub mod utils;
pub mod video;

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
    /// 内置 agent 房间没指定模型（留空或写 `pi`）时用哪个模型。
    /// 写 `供应商/模型` 时按 `[oai.providers]` 取接口；留空则用 oai 的默认模型。
    pub(crate) agent_default_model: String,
    /// 单次回复的总时间预算。
    request_timeout_seconds: u64,
    /// 单次模型请求静默多少秒算卡死（内置 agent 一次性拿完整回复，
    /// 没有中间事件可看，所以这是单次请求的上限）；卡住且还没动过工具时自动重来一次。
    /// 置 0 关闭。
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
    /// 走 Suno 文生歌的房间模型关键字（同样是不区分大小写的子串匹配）。
    pub(crate) music_models: Vec<String>,
    /// Suno 版本（提交时的 `mv` 字段）。站点接入更新的版本时改这里。
    pub(crate) music_version: String,
    /// 生成的歌怎么发进群：`file`（群文件）、`voice`（语音气泡）、`both`（先文件再语音）。
    pub(crate) music_send: String,
    /// 走 OpenAI 视频任务接口的房间模型关键字。
    pub(crate) video_models: Vec<String>,
    /// 没写 `--秒数` 时的默认时长。视频按秒计费，短一点更省。
    pub(crate) video_seconds: u32,
    /// 音乐、视频这类异步任务房间的等待上限；它们出成品常常要几分钟。
    pub(crate) media_timeout_seconds: u64,
    /// 可选供应商表：模型写 `供应商/模型` 时按名字取这里的接口与密钥。
    /// 不配也不影响既有房间——不带前缀的仍走 `oai` 默认接口。
    pub(crate) providers: HashMap<String, ProviderConfig>,
    /// 联网搜索：给内置 agent 房间的 `web_search` / `web_fetch`。
    /// 默认关闭，群聊搭话另有 `[ambient] search_enabled`，两者互不影响。
    pub(crate) search: search::SearchConfig,
}

impl Default for OaiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            agent_default_model: "deepseek/deepseek-flash".to_string(),
            request_timeout_seconds: 300,
            pi_stall_seconds: 180,
            plain_text_max_chars: 120,
            show_trace_footer: true,
            model_filter: utils::ModelFilterConfig::default(),
            image_models: images::DEFAULT_IMAGE_MODELS
                .iter()
                .map(|keyword| (*keyword).to_string())
                .collect(),
            music_models: music::DEFAULT_MUSIC_MODELS
                .iter()
                .map(|keyword| (*keyword).to_string())
                .collect(),
            music_version: music::DEFAULT_VERSION.to_string(),
            music_send: "both".to_string(),
            video_models: video::DEFAULT_VIDEO_MODELS
                .iter()
                .map(|keyword| (*keyword).to_string())
                .collect(),
            video_seconds: 5,
            media_timeout_seconds: 900,
            providers: HashMap::new(),
            search: search::SearchConfig::default(),
        }
    }
}

impl OaiConfig {
    pub(crate) fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.request_timeout_seconds.clamp(30, 1_800))
    }

    /// 单次模型请求静默多久算卡死；`None` 表示不看。
    pub(crate) fn pi_stall(&self) -> Option<std::time::Duration> {
        (self.pi_stall_seconds > 0)
            .then(|| std::time::Duration::from_secs(self.pi_stall_seconds.max(20)))
    }

    /// 内置 agent 房间没写模型时用的默认模型；留空返回 `None`，由调用方退到
    /// oai config.json 里的 `default_model`。
    pub(crate) fn agent_default_model(&self) -> Option<&str> {
        let model = self.agent_default_model.trim();
        (!model.is_empty()).then_some(model)
    }

    pub(crate) fn plain_text_max_chars(&self) -> usize {
        self.plain_text_max_chars
    }

    /// 音乐、视频这类异步任务的等待上限；太短会白等一场，太长又占着会话不放。
    pub(crate) fn media_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.media_timeout_seconds.clamp(60, 3_600))
    }

    /// 提交 Suno 时用的版本；留空回落到内置默认值。
    pub(crate) fn music_version(&self) -> String {
        let version = self.music_version.trim();
        if version.is_empty() {
            music::DEFAULT_VERSION.to_string()
        } else {
            version.to_string()
        }
    }

    pub(crate) fn music_send(&self) -> music::SendMode {
        music::SendMode::parse(&self.music_send)
    }

    /// 视频默认时长：只收 1..=30 秒，越界或为 0 时回到内置默认值。
    pub(crate) fn video_seconds(&self) -> u32 {
        match self.video_seconds {
            1..=30 => self.video_seconds,
            _ => 5,
        }
    }

    pub(crate) fn show_trace_footer(&self) -> bool {
        self.show_trace_footer
    }
}

pub fn default_config() -> Value {
    build_config(OaiConfig::default())
}

/// 确保模型接口配置（`data/oai/config.json`）已加载。
///
/// 房间插件与群聊搭话插件共用这一份配置，任一插件启动时都可以先把管理器建好；
/// 已经建好就原样返回。密钥与接口地址只存在这里，两边不会各存一份。
pub(crate) async fn ensure_manager() -> Result<Arc<data::Manager>, PluginError> {
    if let Some(mgr) = MANAGER.get() {
        return Ok(mgr.clone());
    }
    let dir = get_data_dir("oai").await?;
    let mgr = Arc::new(data::Manager::new(dir));
    let _ = MANAGER.set(mgr);
    Ok(MANAGER.get().expect("刚刚 set 过").clone())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let mgr = ensure_manager().await?;

        // 尝试预加载模型列表
        let filter = crate::plugins::get_config_or_default::<OaiConfig>(&ctx, "oai").model_filter;
        tokio::spawn(async move {
            if let Err(e) = mgr.fetch_models(&filter).await {
                warn!(target: "Plugin/OAI", "初始化获取模型列表失败: {}", e);
            } else {
                info!(target: "Plugin/OAI", "初始化获取模型列表成功");
            }
        });

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
        // 要留给随后的群聊搭话插件观察，事件继续往后传。
        let raw_text = match extract_clean_text(&ctx) {
            Some(t) => t,
            None => return Ok(Some(ctx)),
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

        // 不属于任何指令的普通群聊：事件继续向后传递，交给随后的搭话插件观察。
        Ok(Some(ctx))
    })
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <OaiConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
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
    fn agent_default_model_is_optional_and_legacy_config_remains_loadable() {
        // 旧配置里那些已经不用的键（如 pi_command）不该让整份配置解析失败。
        let config: OaiConfig =
            toml::from_str("pi_command = 'pi'\nharness_rooms = ['pi']").unwrap();
        assert_eq!(config.agent_default_model(), Some("deepseek/deepseek-flash"));
        let config: OaiConfig = toml::from_str("agent_default_model = 'deepseek/deepseek-flash'").unwrap();
        assert_eq!(config.agent_default_model(), Some("deepseek/deepseek-flash"));
        let config: OaiConfig = toml::from_str("agent_default_model = '  '").unwrap();
        assert_eq!(config.agent_default_model(), None);
    }

    #[test]
    fn provider_table_is_optional_and_parses_when_present() {
        // 旧配置没有 providers 也能照常加载。
        let legacy: OaiConfig = toml::from_str("agent_default_model = ''").unwrap();
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

