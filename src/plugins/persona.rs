use crate::adapters::onebot::{LockedWriter, api, send_msg};
use crate::config::build_config;
use crate::event::{Context, EventType, MessageEvent};
use crate::message::Message;
use crate::plugins::{PluginError, get_config, get_data_dir};
use chrono::Timelike;
use futures_util::future::BoxFuture;
use simd_json::OwnedValue;
use simd_json::base::{ValueAsArray, ValueAsScalar};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::sync::Arc;
use std::time::Duration;
use toml::Value;

pub mod chat;
pub mod data;
pub mod memory;
pub mod prompt;
pub mod types;

use data::{MANAGER, Manager};
use types::{PersonaConfig, RecentMsg, ReplyResult};

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

        // 周期性落盘
        let mgr_save = mgr.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                mgr_save.save().await;
            }
        });

        Ok(())
    })
}

pub fn on_connected(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        let cfg: PersonaConfig = match get_config(&ctx, "persona") {
            Some(c) => c,
            None => return Ok(Some(ctx)),
        };
        if !cfg.enabled || !cfg.initiate_enabled {
            return Ok(Some(ctx));
        }
        let mgr = match MANAGER.get() {
            Some(m) => m.clone(),
            None => return Ok(Some(ctx)),
        };

        let interval = Duration::from_secs(cfg.initiate_check_interval_mins.max(1) * 60);
        let ctx_owned = ctx.clone();
        let writer_owned = writer.clone();

        ctx.scheduler.add_interval(interval, move || {
            let ctx = ctx_owned.clone();
            let writer = writer_owned.clone();
            let mgr = mgr.clone();
            async move {
                let cfg: PersonaConfig = match get_config(&ctx, "persona") {
                    Some(c) => c,
                    None => return,
                };
                if !cfg.enabled || !cfg.initiate_enabled {
                    return;
                }
                if is_sleeping(&cfg) {
                    return;
                }

                // 选群：明确白名单优先；否则用我们已记录过状态的群
                let candidate_groups: Vec<String> = if !cfg.allow_groups.is_empty() {
                    cfg.allow_groups.iter().map(|g| g.to_string()).collect()
                } else {
                    mgr.state.read().await.groups.keys().cloned().collect()
                };

                for group_key in candidate_groups {
                    let Ok(group_id) = group_key.parse::<i64>() else {
                        continue;
                    };
                    try_initiate(&cfg, &mgr, &ctx, &writer, &group_key, group_id).await;
                }
            }
        });

        info!(target: "Persona", "主动话题任务已启动 (每 {} 分钟一次)", cfg.initiate_check_interval_mins);
        Ok(Some(ctx))
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

fn random_jitter_ms(base: u64, jitter: u64) -> u64 {
    use rand::Rng;
    if jitter == 0 {
        return base;
    }
    base.saturating_add(rand::rng().random_range(0..=jitter))
}

fn starts_with_command_prefix(text: &str, prefixes: &[String]) -> bool {
    let t = text.trim_start();
    prefixes.iter().any(|p| !p.is_empty() && t.starts_with(p))
}

fn is_sleeping(cfg: &PersonaConfig) -> bool {
    if cfg.sleep_start_hour == cfg.sleep_end_hour {
        return false;
    }
    let h = chrono::Local::now().hour();
    let s = cfg.sleep_start_hour;
    let e = cfg.sleep_end_hour;
    if s < e { h >= s && h < e } else { h >= s || h < e }
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

        // BeforeSend：把 bot（含本插件外的其他插件）发出的群消息也记入上下文
        if let EventType::BeforeSend(packet) = &ctx.event {
            if let Some(gid) = packet.group_id() {
                let group_key = gid.to_string();
                let in_scope = cfg.allow_groups.is_empty() || cfg.allow_groups.contains(&gid);
                if in_scope
                    && let Some(msg_value) = packet.message()
                {
                    let text = extract_text_from_packet(msg_value);
                    if !text.is_empty() {
                        mgr.append_recent(
                            &group_key,
                            RecentMsg {
                                sender_id: ctx.bot.login_user.id.clone(),
                                sender_name: cfg.nickname.clone(),
                                text,
                                ts: memory::now_secs(),
                                message_id: 0,
                                is_self: true,
                                mentions_self: false,
                            },
                            cfg.context_window,
                        )
                        .await;
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
        let message_id = event_view.message_id();
        let now_ts = memory::now_secs();

        // 全局用户档案：跨群跨改名都能识别
        mgr.touch_user(&sender_id, &sender_name).await;

        // 任何情况下都先把消息写入上下文
        mgr.append_recent(
            &group_key,
            RecentMsg {
                sender_id: sender_id.clone(),
                sender_name: sender_name.clone(),
                text: text.clone(),
                ts: now_ts,
                message_id,
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
            .cooldown_ok(
                &group_key,
                cfg.min_reply_interval_secs,
                cfg.max_replies_per_hour,
            )
            .await
        {
            return Ok(Some(ctx));
        }

        // 本地概率筛
        let mut prob = if mentions_self {
            cfg.mention_reply_probability
        } else {
            cfg.base_reply_probability
        };
        if is_sleeping(&cfg) {
            prob *= cfg.sleep_reply_multiplier;
        }
        if !dice(prob) {
            return Ok(Some(ctx));
        }

        // 防并发
        if !mgr.try_acquire(&group_key).await {
            return Ok(Some(ctx));
        }

        let trigger = RecentMsg {
            sender_id,
            sender_name,
            text,
            ts: now_ts,
            message_id,
            is_self: false,
            mentions_self,
        };

        let cfg_clone = cfg.clone();
        let mgr_clone = mgr.clone();
        let writer_clone = writer.clone();
        let ctx_clone = ctx.clone();
        let gk = group_key.clone();
        let bot_id_clone = bot_id.clone();

        tokio::spawn(async move {
            let result = run_reply_pipeline(
                &cfg_clone,
                &mgr_clone,
                &ctx_clone,
                &writer_clone,
                &gk,
                group_id,
                &bot_id_clone,
                trigger,
            )
            .await;
            if let Err(e) = result
                && cfg_clone.log_decisions
            {
                warn!(target: "Persona", "回复流程结束: {}", e);
            }
            mgr_clone.release(&gk).await;
        });

        Ok(Some(ctx))
    })
}

async fn try_initiate(
    cfg: &PersonaConfig,
    mgr: &Arc<Manager>,
    ctx: &Context,
    writer: &LockedWriter,
    group_key: &str,
    group_id: i64,
) {
    let now = memory::now_secs();
    let (quiet_secs, last_initiate_at) = {
        let s = mgr.state.read().await;
        let g = match s.groups.get(group_key) {
            Some(g) => g,
            None => return,
        };
        (
            (now - g.last_msg_at).max(0),
            g.last_initiate_at,
        )
    };

    let quiet_mins = quiet_secs / 60;
    if quiet_mins < cfg.initiate_min_quiet_minutes as i64 {
        return;
    }
    if quiet_secs > (cfg.initiate_max_quiet_hours as i64) * 3600 {
        return;
    }
    // 距离上次自启不到 90 分钟就别再开口
    if (now - last_initiate_at) < 90 * 60 {
        return;
    }
    if !mgr
        .cooldown_ok(
            group_key,
            cfg.min_reply_interval_secs,
            cfg.max_replies_per_hour,
        )
        .await
    {
        return;
    }
    if !dice(cfg.initiate_probability) {
        return;
    }
    if !mgr.try_acquire(group_key).await {
        return;
    }

    let cfg_c = cfg.clone();
    let mgr_c = mgr.clone();
    let ctx_c = ctx.clone();
    let writer_c = writer.clone();
    let gk = group_key.to_string();
    let bot_id = ctx.bot.login_user.id.clone();

    tokio::spawn(async move {
        let res = run_initiate_pipeline(
            &cfg_c,
            &mgr_c,
            &ctx_c,
            &writer_c,
            &gk,
            group_id,
            &bot_id,
        )
        .await;
        if let Err(e) = res
            && cfg_c.log_decisions
        {
            warn!(target: "Persona", "主动话题流程结束: {}", e);
        }
        mgr_c.release(&gk).await;
    });
}

async fn run_reply_pipeline(
    cfg: &PersonaConfig,
    mgr: &Arc<Manager>,
    ctx: &Context,
    writer: &LockedWriter,
    group_key: &str,
    group_id: i64,
    self_id: &str,
    trigger: RecentMsg,
) -> anyhow::Result<()> {
    if cfg.api_key.is_empty() || cfg.api_base.is_empty() {
        return Err(anyhow::anyhow!("API 未配置"));
    }

    // 决策（mention 时按配置可跳过）
    let need_decide = !(trigger.mentions_self && cfg.skip_decide_when_mentioned);
    if need_decide {
        let (decide_sys, decide_user) = {
            let s = mgr.state.read().await;
            let g = s.groups.get(group_key).cloned().unwrap_or_default();
            prompt::build_decide_prompt(cfg, &g, &s.users, self_id, &trigger)
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
    }

    // 生成
    let (state_snap, reply_sys, reply_user) = {
        let s = mgr.state.read().await;
        let g = s.groups.get(group_key).cloned().unwrap_or_default();
        let (sys, usr) = prompt::build_reply_prompt(cfg, &g, &s.users, self_id, &trigger);
        (g, sys, usr)
    };
    let _ = state_snap;

    let result = chat::generate(cfg, &reply_sys, &reply_user).await?;

    // 节流二次校验
    if !mgr
        .cooldown_ok(group_key, cfg.min_reply_interval_secs, cfg.max_replies_per_hour)
        .await
    {
        return Ok(());
    }

    let did_send = dispatch_result(
        cfg,
        ctx,
        writer,
        group_id,
        Some(&trigger),
        &result,
    )
    .await;

    apply_after_effects(
        cfg,
        mgr,
        group_key,
        Some(&trigger),
        &result,
        did_send,
    )
    .await;

    Ok(())
}

async fn run_initiate_pipeline(
    cfg: &PersonaConfig,
    mgr: &Arc<Manager>,
    ctx: &Context,
    writer: &LockedWriter,
    group_key: &str,
    group_id: i64,
    self_id: &str,
) -> anyhow::Result<()> {
    if cfg.api_key.is_empty() || cfg.api_base.is_empty() {
        return Err(anyhow::anyhow!("API 未配置"));
    }
    let (sys, usr) = {
        let s = mgr.state.read().await;
        let g = s.groups.get(group_key).cloned().unwrap_or_default();
        prompt::build_initiate_prompt(cfg, &g, &s.users, self_id)
    };
    let result = chat::generate(cfg, &sys, &usr).await?;

    if result.messages.is_empty() && result.react_emoji_id.is_none() {
        return Ok(());
    }
    if cfg.log_decisions {
        info!(target: "Persona", "[{}] 主动话题: {:?}", group_id, result.messages);
    }
    let did_send = dispatch_result(cfg, ctx, writer, group_id, None, &result).await;

    {
        let mut s = mgr.state.write().await;
        let g = s.groups.entry(group_key.to_string()).or_default();
        g.last_initiate_at = memory::now_secs();
    }

    apply_after_effects(cfg, mgr, group_key, None, &result, did_send).await;
    Ok(())
}

async fn dispatch_result(
    cfg: &PersonaConfig,
    ctx: &Context,
    writer: &LockedWriter,
    group_id: i64,
    trigger: Option<&RecentMsg>,
    result: &ReplyResult,
) -> bool {
    // 优先：表情回应（仅在有 trigger 时可用）
    if cfg.allow_emoji_react
        && let Some(emoji_id) = result.react_emoji_id
        && let Some(t) = trigger
        && t.message_id != 0
    {
        let _ = api::set_msg_emoji_like(
            ctx,
            writer.clone(),
            t.message_id as i32,
            emoji_id,
            true,
        )
        .await;
        return true;
    }

    let segments: Vec<String> = result
        .messages
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(cfg.max_segments.max(1))
        .collect();

    if segments.is_empty() {
        return false;
    }

    let trigger_user_id: Option<i64> = trigger
        .and_then(|t| t.sender_id.parse::<i64>().ok())
        .filter(|id| *id != 0);
    let trigger_msg_id: Option<i64> = trigger.map(|t| t.message_id).filter(|id| *id != 0);

    for (i, text) in segments.iter().enumerate() {
        let mut msg = Message::new();
        let is_first = i == 0;
        if is_first {
            if cfg.allow_reply_quote
                && result.reply_first
                && let Some(id) = trigger_msg_id
            {
                msg = msg.reply(id);
            }
            if cfg.allow_at_user
                && result.at_first
                && let Some(uid) = trigger_user_id
            {
                msg = msg.at(uid).text(" ");
            }
        }
        msg = msg.text(text.clone());

        let delay = if is_first {
            random_jitter_ms(
                cfg.typing_delay_ms + (text.chars().count() as u64) * 60,
                500,
            )
        } else {
            random_jitter_ms(
                cfg.inter_segment_delay_ms + (text.chars().count() as u64) * 50,
                400,
            )
        };
        tokio::time::sleep(Duration::from_millis(delay.min(6000))).await;

        let _ = send_msg(ctx, writer.clone(), Some(group_id), None, msg).await;
    }

    true
}

async fn apply_after_effects(
    cfg: &PersonaConfig,
    mgr: &Arc<Manager>,
    group_key: &str,
    trigger: Option<&RecentMsg>,
    result: &ReplyResult,
    did_send: bool,
) {
    {
        let mut s = mgr.state.write().await;
        let shift = result.mood_shift.clamp(-0.3, 0.3);
        let g = s.groups.entry(group_key.to_string()).or_default();
        g.mood = (g.mood * 0.9 + shift).clamp(-1.0, 1.0);
        if !result.remember_global.trim().is_empty() {
            memory::add_memory(
                &mut g.memories,
                &result.remember_global,
                cfg.max_memories,
                cfg.memory_half_life_days,
            );
        }

        if !result.remember_user.trim().is_empty()
            && let Some(t) = trigger
            && !t.sender_id.is_empty()
        {
            if let Some(prof) = s.users.get_mut(&t.sender_id) {
                memory::add_memory(
                    &mut prof.notes,
                    &result.remember_user,
                    cfg.max_user_notes,
                    cfg.memory_half_life_days,
                );
            }
        }
    }

    if did_send {
        mgr.record_reply(group_key).await;
    }
}
