use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::event::Context;
use crate::message::Message;
use anyhow::Result;
use cdp_html_shot::{Browser, Viewport};
use rand::seq::IndexedRandom;
use shindan_maker::{ShindanClient, ShindanDomain};
use simd_json::OwnedValue;
use std::time::Duration;
use tokio::time;

use super::config::{PluginConfig, ShindanDefinition};
use super::storage::Storage;
use super::utils::{get_target_name_and_id, reply_text};

// --- Thread-Safe Data Fetchers ---

pub async fn fetch_info(domain: ShindanDomain, id: &str) -> Result<(String, String)> {
    let id = id.to_string();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!(e))?;
        rt.block_on(async move {
            let client = ShindanClient::new(domain).map_err(|e| anyhow::anyhow!(e))?;
            client
                .get_title_with_description(&id)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        })
    });
    handle.await.map_err(|e| anyhow::anyhow!(e))?
}

async fn fetch_segments(
    domain: ShindanDomain,
    id: &str,
    name: &str,
) -> Result<shindan_maker::Segments> {
    let id = id.to_string();
    let name = name.to_string();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!(e))?;
        rt.block_on(async move {
            let client = ShindanClient::new(domain).map_err(|e| anyhow::anyhow!(e))?;
            client
                .get_segments(&id, &name)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        })
    });
    handle.await.map_err(|e| anyhow::anyhow!(e))?
}

async fn fetch_html(domain: ShindanDomain, id: &str, name: &str) -> Result<String> {
    let id = id.to_string();
    let name = name.to_string();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!(e))?;
        rt.block_on(async move {
            let client = ShindanClient::new(domain).map_err(|e| anyhow::anyhow!(e))?;
            client
                .get_html_str(&id, &name)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        })
    });
    handle.await.map_err(|e| anyhow::anyhow!(e))?
}

// --- Execution Logic ---
#[allow(clippy::too_many_arguments)]
pub async fn handle_shindan_exec(
    ctx: &Context,
    writer: LockedWriter,
    params: &[&str],
    args: &[OwnedValue],
    domain: ShindanDomain,
    storage: &Storage,
    config: &PluginConfig,
    is_random: bool,
) -> Result<()> {
    let list = storage.get_shindans();
    if list.is_empty() {
        reply_text(ctx, writer, "列表为空。").await?;
        return Ok(());
    }

    let shindan = if is_random {
        list.choose(&mut rand::rng()).unwrap()
    } else {
        unreachable!("Should use run_specific_shindan for non-random");
    };

    if config.random_return_command {
        let msg_evt = ctx.as_message().unwrap();
        let mut msg = Message::new();
        msg = msg.reply(msg_evt.message_id()).text(&shindan.command);
        match send_msg(ctx, writer.clone(), msg_evt.group_id(), None, msg).await {
            Ok(_) => {}
            Err(e) => {
                return Err(anyhow::anyhow!(e));
            }
        }
    }

    execute_shindan(ctx, writer, shindan, params, args, domain, storage, config).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_specific_shindan(
    ctx: &Context,
    writer: LockedWriter,
    cmd: &str,
    params: &[&str],
    args: &[OwnedValue],
    domain: ShindanDomain,
    storage: &Storage,
    config: &PluginConfig,
) -> Result<()> {
    let list = storage.get_shindans();
    if let Some(s) = list.iter().find(|s| s.command == cmd) {
        execute_shindan(ctx, writer, s, params, args, domain, storage, config).await
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_shindan(
    ctx: &Context,
    writer: LockedWriter,
    shindan: &ShindanDefinition,
    params: &[&str],
    args: &[OwnedValue],
    domain: ShindanDomain,
    storage: &Storage,
    _config: &PluginConfig,
) -> Result<()> {
    // 1. 确定名字
    let msg_evt = ctx.as_message().unwrap();
    let (target_name, _target_id) = get_target_name_and_id(ctx, writer.clone(), params, args).await;

    // 2. 确定模式 (优先 params 中的 -t/-i)
    let mode_str = if params.contains(&"-t") {
        "text"
    } else if params.contains(&"-i") {
        "image"
    } else {
        &shindan.mode
    };

    // 3. 记录统计
    let sender_name = msg_evt.sender_name();
    let sender_id = msg_evt.user_id();
    storage
        .record_usage(&ctx.db, sender_id, sender_name, &shindan.id)
        .await;

    // 4. 执行
    if mode_str == "text" {
        match fetch_segments(domain, &shindan.id, &target_name).await {
            Ok(segments) => {
                let mut msg = Message::new();
                msg = msg.reply(msg_evt.message_id());
                // Convert segments
                for seg in segments.0 {
                    match seg.type_.as_str() {
                        "text" => {
                            msg = msg.text(seg.data["text"].as_str().unwrap_or(""));
                        }
                        "image" => {
                            if let Some(url) = seg.data["url"].as_str() {
                                msg = msg.image(url);
                            }
                        }
                        _ => {}
                    }
                }
                match send_msg(ctx, writer, msg_evt.group_id(), Some(sender_id), msg).await {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(e));
                    }
                }
            }
            Err(e) => {
                error!(target: "Shindan", "Text mode error: {}", e);
                reply_text(ctx, writer, "❌ 神断失败：网络错误").await?;
            }
        }
    } else {
        // Image Mode
        match fetch_html(domain, &shindan.id, &target_name).await {
            Ok(html) => {
                // Render
                let browser = Browser::instance().await;
                let tab = browser.new_tab().await.map_err(|e| anyhow::anyhow!(e))?;

                // 先设置初始视口并加载内容，确保布局初始化正确
                tab.set_viewport(&Viewport::new(1200, 800)).await?;
                tab.set_content(&html).await?;

                // Check chart.js logic
                if html.contains("canvas_block") {
                    time::sleep(Duration::from_secs(2)).await;
                }

                // 在内容加载后计算实际高度
                let height_js =
                    "Math.max(document.body.scrollHeight, document.documentElement.scrollHeight)";
                let page_height = tab.evaluate(height_js).await?.as_f64().unwrap_or(800.0) as u32;

                // 根据实际高度调整视口，确保长图能被完整渲染
                // 适当增加 max clamp (5000 -> 10000) 以适应超长结果
                let final_height = (page_height + 100).clamp(100, 10000);
                tab.set_viewport(&Viewport::new(1200, final_height)).await?;

                let elem = tab.find_element("#title_and_result").await?;
                let b64 = elem.screenshot().await?;
                let _ = tab.close().await;

                let mut msg = Message::new();

                msg = msg.image(format!("base64://{}", b64));

                match send_msg(ctx, writer, msg_evt.group_id(), Some(sender_id), msg).await {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(e));
                    }
                }
            }
            Err(e) => {
                error!(target: "Shindan", "Image mode error: {}", e);
                reply_text(ctx, writer, "❌ 神断失败：网络错误").await?;
            }
        }
    }

    Ok(())
}
