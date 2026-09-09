//! 词意：每日一个两字词，用「语义排名」把它逼出来。
//!
//! 一局的作用域是「会话」而非「群」：群聊各群一局，私聊各人一局，
//! 两处都能玩，互不干扰（作用域键见 `scope_of`）。
//!
//! 呈现方式：盘面、揭晓、排行榜、玩法说明都排版成一张宣纸风的卡片图，
//! 一局下来翻回去看历次提示不必在聊天记录里大海捞针；「不在词库中」这类
//! 即时纠错仍走纯文本——它要的是快，不是好看。
//!
//! 出图有三层，逐层兜底，任何一层塌了功能都不受影响：
//!   1. **网页卡片**（`web.rs` + `res/cards/ciyi.css`）：交给无头浏览器排版，
//!      字距、折行、省略号、弹性列宽都由排版引擎负责，版面最经得起看；
//!   2. **原生绘制**（`card.rs` + `painter.rs`）：浏览器缺席或截图失败时顶上，
//!      文字由 ab_glyph 直接光栅化，不依赖任何外部进程；
//!   3. **纯文本**：连一个可用字体都没有时的最后一手。

pub mod card;
pub mod config;
pub mod data;
pub mod engine;
pub mod entity;
pub mod painter;
pub mod view;
pub mod web;

use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::{get_prefixes, match_command};
use crate::config::build_config;
use crate::event::{Context, MessageEvent};
use crate::message::Message;
use crate::plugins::ciyi::config::CiYiConfig;
use crate::plugins::ciyi::entity::{record as record_entity, state as state_entity};
use crate::plugins::ciyi::view::Reply;
use crate::plugins::{PluginError, get_config};
use base64::{Engine, engine::general_purpose::STANDARD};
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
            .execute_raw(builder.build(create_state_table.if_not_exists()))
            .await
        {
            crate::warn!(target: "Plugin/CiYi", "Init state table error: {}", e);
        }

        // 2. 创建 Record 表
        let mut create_record_table = schema.create_table_from_entity(record_entity::Entity);
        if let Err(e) = db
            .execute_raw(builder.build(create_record_table.if_not_exists()))
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

        if let Err(e) = db.execute_raw(builder.build(&idx_group_user)).await {
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

        let group_id = msg_event.group_id();
        let scope = scope_of(&msg_event);
        let user_id = msg_event.user_id();
        let text = msg_event.text().trim();

        if text.is_empty() {
            return Ok(Some(ctx));
        }

        let config: CiYiConfig = get_config(&ctx, "ciyi").unwrap_or_default();

        // A. 无前缀猜测。这一段是中间件：每一条消息都会流过，
        //    所以三道闸门缺一不可——两个字、词在词库里、本会话正有一局没结束。
        //    先查词库再查库：绝大多数两字消息在第一步就被挡下，省掉一次查询；
        //    开局只认「词意猜测」指令，随口两个字既不该开局，也不该收到任何回应。
        if text.chars().count() == 2
            && data::get_all_words().contains(text)
            && engine::direct_guess_open(&ctx.db, scope).await
        {
            let username = msg_event.sender_name().to_string();
            let reply =
                engine::guess_word(&ctx.db, scope, user_id, &username, text, &config).await;

            send_response(&ctx, writer, group_id, user_id, reply, &config).await?;
            return Ok(None); // 阻止后续处理
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
                                    &ctx.db, scope, user_id, &username, arg, &config,
                                )
                                .await
                            }
                        }
                        "rank_group" => {
                            let label = if group_id.is_some() { "本群" } else { "本会话" };
                            engine::get_channel_leaderboard(
                                &ctx.db,
                                scope,
                                label,
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
                                scope,
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

/// 一局游戏的作用域键。
///
/// 群聊用群号，私聊用**负的**用户号。QQ 的群号与用户号都是正整数，取负不会撞车，
/// 于是私聊各开各的一局，而群聊的键还是原来那个值——数据库结构与既有存档都不用动。
fn scope_of(msg: &MessageEvent<'_>) -> i64 {
    match msg.group_id() {
        Some(gid) => gid,
        None => -msg.user_id(),
    }
}

/// 构建并发送回复：能出图的出图，出不来就发文本。
///
/// 引用与 @ 的行为对图文一致——回的还是「你刚才那条」，
/// 只是内容从一段文本换成了一张卡片。`group_id` 为 None 即私聊，走私信。
async fn send_response(
    ctx: &Context,
    writer: LockedWriter,
    group_id: Option<i64>,
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

    let prefix = get_prefixes(ctx).first().cloned().unwrap_or_default();
    msg = match render_card(ctx, &reply, config, &prefix).await {
        Some(b64) => msg.image(format!("base64://{b64}")),
        None => msg.text(reply.to_text()),
    };

    send_msg(ctx, writer, group_id, Some(user_id), msg).await?;
    Ok(())
}

/// 排版成 PNG：先试网页卡片，退回原生绘制，再退回纯文本。
///
/// 关掉图片或内容不值得出图（`Notice`）时直接返回 None。两条出图路径共用
/// 同一份数据，所以任何一层被跳过，图上说的都还是同一件事。
/// 附带把每次发出去的图落盘到 `CIYI_CARD_DEBUG_DUMP/last_sent.png`，
/// 便于排查「图片底部被截」等疑似被 QQ 二次处理的问题。
async fn render_card(
    ctx: &Context,
    reply: &Reply,
    config: &CiYiConfig,
    prefix: &str,
) -> Option<String> {
    if !config.plugin.image_enabled || !reply.wants_card() {
        return None;
    }

    let browser_path = ctx.config.read().unwrap().browser_path.clone();
    let b64 = match web::render(reply, prefix, config.plugin.image_scale, browser_path.as_deref())
        .await
    {
        Ok(b64) => b64,
        Err(e) => {
            crate::warn!(target: LOG_TARGET, "网页卡片出图失败（{e}），改用原生绘制");
            match card::render(reply, prefix, config.plugin.image_scale) {
                Some(b64) => b64,
                None => {
                    crate::warn!(target: LOG_TARGET, "原生绘制也失败（字体不可用？），本次改发纯文本");
                    return None;
                }
            }
        }
    };

    if let Ok(dir) = std::env::var("CIYI_CARD_DEBUG_DUMP") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            format!("{dir}/last_sent.png"),
            STANDARD.decode(&b64).unwrap_or_default(),
        );
    }
    crate::info!(
        target: LOG_TARGET,
        "卡片已渲染: base64 {} 字符，原始 {} 字节",
        b64.len(),
        b64.len() * 3 / 4,
    );
    Some(b64)
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <CiYiConfig as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}
