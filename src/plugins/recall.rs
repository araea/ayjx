use crate::adapters::satori::LockedWriter;
use crate::adapters::satori::api;
use crate::command::match_command;
use crate::config::build_config;
use crate::event::Context;
use crate::plugins::PluginError;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use toml::Value;

#[derive(Serialize, Deserialize)]
struct Config {
    enabled: bool,
}

pub fn default_config() -> Value {
    build_config(Config { enabled: true })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        if let Some(cmd) = match_command(&ctx, "撤回")
            && let Some(reply_id_str) = cmd.reply_id
        {
            let msg = match ctx.as_message() {
                Some(m) => m,
                None => return Ok(Some(ctx)),
            };

            let command_msg_id = msg.message_id();

            if let Ok(target_id) = reply_id_str.parse::<i64>() {
                let _ = api::delete_msg(&ctx, writer.clone(), target_id).await;

                let _ = api::delete_msg(&ctx, writer, command_msg_id).await;

                return Ok(None);
            }
        }

        Ok(Some(ctx))
    })
}
