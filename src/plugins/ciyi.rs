//! 词意：每日一个两字词，全群一起用「语义排名」把它逼出来。
//!
//! 呈现方式：盘面、揭晓、排行榜、玩法说明都排版成一张宣纸风的卡片图
//! （见 `card.rs`），一局下来翻回去看历次提示不必在聊天记录里大海捞针；
//! 「不在词库中」这类即时纠错仍走纯文本——它要的是快，不是好看。
//! 截图失败（浏览器不可用等）自动退回文本，功能不受影响。

pub mod card;
pub mod config;
pub mod data;
pub mod engine;
pub mod entity;
pub mod view;

use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::command::{get_prefixes, match_command};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::ciyi::config::CiYiConfig;
use crate::plugins::ciyi::entity::{record as record_entity, state as state_entity};
use crate::plugins::ciyi::view::Reply;
use crate::plugins::{PluginError, get_config};
use futures_util::future::BoxFuture;
use sea_orm::{ConnectionTrait, Schema};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use toml::Value;

pub const LOG_TARGET: &str = "Plugin/CiYi";

pub fn default_config() -> Value {
    build_config(CiYiConfig::default())
}

pub fn init(ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let db = &ctx.db;
        let builder = db.get_database_backend();
        let schema = Schema::new(builder);

        // 1. 创建 State 表
        let mut create_state_table = schema.create_table_from_entity(state_entity::Entity);
        if let Err(e) = db
            .execute(builder.build(create_state_table.if_not_exists()))
            .await
        {
            crate::warn!(target: "Plugin/CiYi", "Init state table error: {}", e);
        }

        // 2. 创建 Record 表
        let mut create_record_table = schema.create_table_from_entity(record_entity::Entity);
        if let Err(e) = db
            .execute(builder.build(create_record_table.if_not_exists()))
            .await
        {
            crate::warn!(target: "Plugin/CiYi", "Init record table error: {}", e);
        }

        // 3. 创建索引 (针对排行榜查询优化)
        // 索引1: WHERE group_id GROUP BY user_id
        let idx_group_user = sea_orm::sea_query::Index::create()
            .name("idx_ciyi_win_record_group_user")
            .table(record_entity::Entity)
            .col(record_entity::Column::GroupId)
            .col(record_entity::Column::UserId)
            .if_not_exists()
            .to_owned();

        if let Err(e) = db.execute(builder.build(&idx_group_user)).await {
            crate::warn!(target: "Plugin/CiYi", "Init index error: {}", e);
        }

        // crate::info!(target: "Plugin/CiYi", "词意游戏初始化完成 (SQL)");
        Ok(())
    })
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let msg_event = match ctx.as_message() {
            Some(e) => e,
            None => return Ok(Some(ctx)),
        };

        // 仅在群聊中响应
        let group_id = match msg_event.group_id() {
            Some(id) => id,
            None => return Ok(Some(ctx)),
        };
        let user_id = msg_event.user_id();
        let text = msg_event.text().trim();

        if text.is_empty() {
            return Ok(Some(ctx));
        }

        let config: CiYiConfig = get_config(&ctx, "ciyi").unwrap_or_default();

        // A. 直接猜测模式 (两个字)
        if text.chars().count() == 2 {
            let should_direct_guess =
                engine::get_direct_guess_status(&ctx.db, group_id, config.plugin.direct_guess)
                    .await;

            if should_direct_guess {
                let username = msg_event.sender_name().to_string();
                let reply =
                    engine::guess_word(&ctx.db, group_id, user_id, &username, text, &config).await;

                send_response(&ctx, writer, group_id, user_id, reply, &config).await?;
                return Ok(None); // 阻止后续处理
            }
        }

        // B. 指令处理
        let commands = vec![
            (
                vec!["词意帮助", "词意指令", "词意指令列表", "词意帮助列表"],
                "help",
            ),
            (vec!["词意玩法", "词意规则"], "rules"),
            (vec!["词意猜测"], "guess"),
            (vec!["词意榜"], "rank_group"),
            (vec!["词意全榜"], "rank_global"),
            (vec!["切换猜测模式"], "toggle_mode"),
        ];

        for (aliases, action) in commands {
            for alias in aliases {
                if let Some(cmd) = match_command(&ctx, alias) {
                    let response = match action {
                        "help" => Reply::Help,
                        "rules" => Reply::Rules,
                        "guess" => {
                            let arg = cmd
                                .args
                                .first()
                                .and_then(|seg| seg.get("data"))
                                .and_then(|d| d.get_str("text"))
                                .unwrap_or("")
                                .trim();
                            if arg.chars().count() != 2 {
                                Reply::Notice("无效输入，请发送两个字的词语。".to_string())
                            } else {
                                let username = msg_event.sender_name().to_string();
                                engine::guess_word(
                                    &ctx.db, group_id, user_id, &username, arg, &config,
                                )
                                .await
                            }
                        }
                        "rank_group" => {
                            engine::get_channel_leaderboard(
                                &ctx.db,
                                group_id,
                                config.plugin.rank_display,
                            )
                            .await
                        }
                        "rank_global" => {
                            engine::get_global_leaderboard(&ctx.db, config.plugin.rank_display)
                                .await
                        }
                        "toggle_mode" => Reply::Notice(
                            engine::toggle_direct_guess_mode(
                                &ctx.db,
                                group_id,
                                config.plugin.direct_guess,
                            )
                            .await,
                        ),
                        _ => Reply::Notice(String::new()),
                    };

                    if !response.to_text().is_empty() {
                        send_response(&ctx, writer, group_id, user_id, response, &config).await?;
                    }
                    return Ok(None);
                }
            }
        }

        Ok(Some(ctx))
    })
}

/// 构建并发送回复：能出图的出图，出不来就发文本。
///
/// 引用与 @ 的行为对图文一致——群里回的还是「你刚才那条」，
/// 只是内容从一段文本换成了一张卡片。
async fn send_response(
    ctx: &Context,
    writer: LockedWriter,
    group_id: i64,
    user_id: i64,
    reply: Reply,
    config: &CiYiConfig,
) -> Result<(), PluginError> {
    let mut msg = Message::new();

    if config.plugin.quote_user {
        let msg_id = ctx.as_message().map(|m| m.message_id()).unwrap_or(0);
        msg = msg.reply(msg_id);
    }

    if config.plugin.at_user {
        msg = msg.at(user_id).text("\n");
    }

    msg = match render_card(ctx, &reply, config).await {
        Some(b64) => msg.image(format!("base64://{b64}")),
        None => msg.text(reply.to_text()),
    };

    send_msg(ctx, writer, Some(group_id), None, msg).await?;
    Ok(())
}

/// 排版并截图；关掉图片、内容不值得出图、或浏览器不可用时返回 None
async fn render_card(ctx: &Context, reply: &Reply, config: &CiYiConfig) -> Option<String> {
    if !config.plugin.image_enabled || !reply.wants_card() {
        return None;
    }

    let prefix = get_prefixes(ctx).first().cloned().unwrap_or_default();
    let html = card::render(reply, &prefix)?;
    match card::capture(&html, config.plugin.image_scale).await {
        Ok(b64) => Some(b64),
        Err(e) => {
            crate::warn!(target: LOG_TARGET, "卡片渲染失败，本次改发纯文本: {}", e);
            None
        }
    }
}
