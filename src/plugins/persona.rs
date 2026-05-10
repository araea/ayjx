use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::config::build_config;
use crate::event::{Context, EventType, MessageEvent};
use crate::message::Message;
use crate::plugins::{PluginError, get_config, get_data_dir};
use futures_util::future::BoxFuture;
use simd_json::OwnedValue;
use simd_json::base::{ValueAsArray, ValueAsScalar};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::sync::Arc;
use toml::Value;

pub mod chat;
pub mod data;
pub mod memory;
pub mod prompt;
pub mod types;

use data::{MANAGER, Manager};
use types::{PersonaConfig, RecentMsg};

pub fn default_config() -> Value {
    build_config(PersonaConfig::default())
}

pub fn init(_ctx: Context) -> BoxFuture<'static, Result<(), PluginError>> {
    Box::pin(async move {
        let dir = get_data_dir("persona").await?;
        let mgr = Arc::new(Manager::new(dir));

        if MANAGER.set(mgr.clone()).is_err() {
            warn!(target: "Persona", "Manager 已经初始化过");
            return Ok(());
        }

        // 每 5 分钟落盘一次，避免长时间内存中状态丢失
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                mgr.save().await;
            }
        });

        Ok(())
    })
}

fn extract_text_and_mentions(
    event: &OwnedValue,
    bot_id: &str,
    nickname: &str,
) -> (String, bool) {
    let mut text_acc = String::new();
    let mut mentions_self = false;
    let mut found_start = false;

    if let Some(arr) = event.get_array("message") {
        for seg in arr {
            let type_ = seg.get_str("type").unwrap_or("");
            if type_ == "at" {
                let qq = seg
                    .get("data")
                    .and_then(|d| d.get_str("qq"))
                    .map(|s| s.to_string())
                    .or_else(|| {
                        seg.get("data")
                            .and_then(|d| d.get_i64("qq"))
                            .map(|v| v.to_string())
                    });
                if let Some(q) = qq
                    && !bot_id.is_empty()
                    && q == bot_id
                {
                    mentions_self = true;
                }
                continue;
            }
            if type_ == "reply" {
                continue;
            }
            if type_ == "text" {
                let t = seg
                    .get("data")
                    .and_then(|d| d.get_str("text"))
                    .unwrap_or("");
                if !found_start {
                    let t2 = t.trim_start();
                    if t2.is_empty() {
                        continue;
                    }
                    found_start = true;
                    text_acc.push_str(t2);
                } else {
                    text_acc.push_str(t);
                }
            }
        }
    }

    if !nickname.is_empty() && text_acc.contains(nickname) {
        mentions_self = true;
    }

    (text_acc.trim().to_string(), mentions_self)
}

fn extract_text_from_packet(packet_msg: &OwnedValue) -> String {
    let mut s = String::new();
    if let Some(arr) = packet_msg.as_array() {
        for seg in arr {
            if seg.get_str("type") == Some("text")
                && let Some(t) = seg.get("data").and_then(|d| d.get_str("text"))
            {
                s.push_str(t);
            }
        }
    } else if let Some(t) = packet_msg.as_str() {
        s.push_str(t);
    }
    s.trim().to_string()
}

fn dice(prob: f32) -> bool {
    use rand::Rng;
    if prob >= 1.0 {
        return true;
    }
    if prob <= 0.0 {
        return false;
    }
    rand::rng().random::<f32>() < prob
}

fn starts_with_command_prefix(text: &str, prefixes: &[String]) -> bool {
    let t = text.trim_start();
    prefixes.iter().any(|p| !p.is_empty() && t.starts_with(p))
}

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let cfg: PersonaConfig = match get_config(&ctx, "persona") {
            Some(c) => c,
            None => return Ok(Some(ctx)),
        };
        if !cfg.enabled {
            return Ok(Some(ctx));
        }

        let mgr = match MANAGER.get() {
            Some(m) => m.clone(),
            None => return Ok(Some(ctx)),
        };

        // 处理自己即将发出的消息：把它当作上下文记录下来，让 bot 有"自我记忆"
        if let EventType::BeforeSend(packet) = &ctx.event {
            if let Some(gid) = packet.group_id() {
                let group_key = gid.to_string();
                if cfg.allow_groups.is_empty() || cfg.allow_groups.contains(&gid) {
                    if let Some(msg_value) = packet.message() {
                        let text = extract_text_from_packet(msg_value);
                        if !text.is_empty() {
                            mgr.append_recent(
                                &group_key,
                                RecentMsg {
                                    sender_id: ctx.bot.login_user.id.clone(),
                                    sender_name: cfg.nickname.clone(),
                                    text,
                                    ts: memory::now_secs(),
                                    is_self: true,
                                    mentions_self: false,
                                },
                                cfg.context_window,
                            )
                            .await;
                        }
                    }
                }
            }
            return Ok(Some(ctx));
        }

        // 仅处理群聊 OneBot 消息
        let event_view: MessageEvent<'_> = match ctx.as_message() {
            Some(e) => e,
            None => return Ok(Some(ctx)),
        };
        if !event_view.is_group() {
            return Ok(Some(ctx));
        }
        let group_id = match event_view.group_id() {
            Some(g) if g != 0 => g,
            _ => return Ok(Some(ctx)),
        };
        if !cfg.allow_groups.is_empty() && !cfg.allow_groups.contains(&group_id) {
            return Ok(Some(ctx));
        }

        let bot_id = ctx.bot.login_user.id.clone();
        let sender_id = event_view.user_id().to_string();
        if !bot_id.is_empty() && sender_id == bot_id {
            return Ok(Some(ctx));
        }

        let onebot_event = match &ctx.event {
            EventType::Onebot(e) => e,
            _ => return Ok(Some(ctx)),
        };
        let (text, mentions_self) =
            extract_text_and_mentions(onebot_event, &bot_id, &cfg.nickname);
        if text.is_empty() {
            return Ok(Some(ctx));
        }

        let prefixes = {
            let g = ctx.config.read().unwrap();
            g.command_prefix.clone()
        };
        let is_command = starts_with_command_prefix(&text, &prefixes);

        let group_key = group_id.to_string();
        let sender_name = event_view.sender_name().to_string();
        let now_ts = memory::now_secs();

        // 任何情况下都先把消息写入上下文
        mgr.append_recent(
            &group_key,
            RecentMsg {
                sender_id: sender_id.clone(),
                sender_name: sender_name.clone(),
                text: text.clone(),
                ts: now_ts,
                is_self: false,
                mentions_self,
            },
            cfg.context_window,
        )
        .await;

        // 命令直接放行；不参与回复决策
        if is_command {
            return Ok(Some(ctx));
        }

        // 节流
        if !mgr
            .cooldown_ok(&group_key, cfg.min_reply_interval_secs, cfg.max_replies_per_hour)
            .await
        {
            return Ok(Some(ctx));
        }

        // 第一道筛子：本地概率，过滤掉绝大多数无关消息，节省 API 调用
        let prob = if mentions_self {
            cfg.mention_reply_probability
        } else {
            cfg.base_reply_probability
        };
        if !dice(prob) {
            return Ok(Some(ctx));
        }

        // 防并发：同一个群正在生成时不再启动新的回复任务
        if !mgr.try_acquire(&group_key).await {
            return Ok(Some(ctx));
        }

        let trigger = RecentMsg {
            sender_id,
            sender_name,
            text,
            ts: now_ts,
            is_self: false,
            mentions_self,
        };

        let cfg_clone = cfg.clone();
        let mgr_clone = mgr.clone();
        let writer_clone = writer.clone();
        let ctx_clone = ctx.clone();
        let gk = group_key.clone();

        tokio::spawn(async move {
            let result =
                run_reply_pipeline(&cfg_clone, &mgr_clone, &ctx_clone, &writer_clone, &gk, group_id, trigger).await;
            if let Err(e) = result {
                if cfg_clone.log_decisions {
                    warn!(target: "Persona", "回复流程结束: {}", e);
                }
            }
            mgr_clone.release(&gk).await;
        });

        Ok(Some(ctx))
    })
}

async fn run_reply_pipeline(
    cfg: &PersonaConfig,
    mgr: &Arc<Manager>,
    ctx: &Context,
    writer: &LockedWriter,
    group_key: &str,
    group_id: i64,
    trigger: RecentMsg,
) -> anyhow::Result<()> {
    if cfg.api_key.is_empty() || cfg.api_base.is_empty() {
        return Err(anyhow::anyhow!("API 未配置"));
    }

    // 决策阶段
    let (state_snap, decide_sys, decide_user) = {
        let s = mgr.state.read().await;
        let g = s.groups.get(group_key).cloned().unwrap_or_default();
        let (sys, usr) = prompt::build_decide_prompt(cfg, &g, &trigger);
        (g, sys, usr)
    };

    let decide = chat::decide(cfg, &decide_sys, &decide_user).await?;
    if cfg.log_decisions {
        info!(
            target: "Persona",
            "[{}] decide: reply={} urgency={} why={}",
            group_id, decide.reply, decide.urgency, decide.why
        );
    }
    if !decide.reply {
        return Ok(());
    }

    // 双重确认节流（API 期间可能有变化）
    if !mgr
        .cooldown_ok(group_key, cfg.min_reply_interval_secs, cfg.max_replies_per_hour)
        .await
    {
        return Ok(());
    }

    // 回复阶段
    let (reply_sys, reply_user) = prompt::build_reply_prompt(cfg, &state_snap, &trigger);
    let reply = chat::reply(cfg, &reply_sys, &reply_user).await?;

    let text = reply.reply.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }

    // 输入延迟，让回复看起来不像机器人秒回
    if cfg.typing_delay_ms > 0 {
        let extra = (text.chars().count() as u64).saturating_mul(40);
        let total = cfg.typing_delay_ms.saturating_add(extra.min(2500));
        tokio::time::sleep(std::time::Duration::from_millis(total)).await;
    }

    let _ = send_msg(
        ctx,
        writer.clone(),
        Some(group_id),
        None,
        Message::new().text(text.clone()),
    )
    .await;

    // 更新状态：心情漂移、记忆抽取、统计
    {
        let mut s = mgr.state.write().await;
        let g = s.groups.entry(group_key.to_string()).or_default();
        let shift = reply.mood_shift.clamp(-0.3, 0.3);
        g.mood = (g.mood * 0.9 + shift).clamp(-1.0, 1.0);
        if !reply.remember.trim().is_empty() {
            memory::add_memory(g, &reply.remember, cfg.max_memories, cfg.memory_half_life_days);
        }
    }

    mgr.record_reply(group_key).await;
    // 注意：bot 自己的发言会通过 BeforeSend 路径进入 recent，不需要在这里重复 append

    Ok(())
}
