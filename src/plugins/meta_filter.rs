use crate::adapters::onebot::LockedWriter;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::PluginError;
use futures_util::future::BoxFuture;
use serde::Serialize;
use toml::Value;

#[derive(Serialize)]
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
