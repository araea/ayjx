use crate::adapters::satori::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::PluginError;
use futures_util::future::BoxFuture;
use serde::Serialize;
use toml::Value;

#[derive(Serialize, serde::Deserialize)]
struct FilterConfig {
    enabled: bool,
}

pub fn default_config() -> Value {
    build_config(FilterConfig { enabled: true })
}

pub fn handle(
    ctx: Context,
    _writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if let Some(post_type) = ctx.post_type()
            && post_type == "meta_event"
        {
            return Ok(None);
        }
        Ok(Some(ctx))
    })
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <FilterConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}
