use crate::adapters::onebot::{LockedWriter, send_msg};
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{PluginError, get_config};
use anyhow::{Result, anyhow};
use cdp_html_shot::{Browser, CaptureOptions, ImageFormat, Viewport};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time;
use toml::Value;

// ================= Config =================

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ChannelConfig {
    #[serde(default)]
    pub white: Vec<i64>,
    #[serde(default)]
    pub black: Vec<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub enabled: bool,
    pub max_height: u32,
    pub timeout_seconds: u64,
    pub quality: u8,
    pub viewport_width: u32,
    pub device_scale_factor: f64,
    pub ignore_domains: Vec<String>,
    #[serde(default)]
    pub channel: ChannelConfig,
}

pub fn default_config() -> Value {
    build_config(Config {
        enabled: true,
        max_height: 5000,
        timeout_seconds: 30,
        quality: 80,
        viewport_width: 1280,
        device_scale_factor: 1.0,
        ignore_domains: vec![],
        channel: ChannelConfig::default(),
    })
}

// ================= Core Logic =================

async fn capture_url(url: &str, config: &Config, browser_path: Option<String>) -> Result<String> {
    // 使用框架提供的全局浏览器实例
    // 优先使用传入的路径（来自全局配置），若为空则使用默认查找
    let browser = if let Some(path) = browser_path {
        if !path.is_empty() {
            Browser::instance_with_path(path).await
        } else {
            Browser::instance().await
        }
    } else {
        Browser::instance().await
    };

    let tab = browser.new_tab().await?;

    let initial_viewport = Viewport::new(config.viewport_width, 800)
        .with_device_scale_factor(config.device_scale_factor);

    tab.set_viewport(&initial_viewport).await?;

    match time::timeout(Duration::from_secs(config.timeout_seconds), tab.goto(url)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = tab.close().await;
            return Err(anyhow!("Navigate failed: {}", e));
        }
        Err(_) => {
            let _ = tab.close().await;
            return Err(anyhow!("Page load timeout"));
        }
    };

    // 等待页面渲染
    time::sleep(Duration::from_millis(1000)).await;

    // 计算页面高度
    let height_js = "Math.max(document.body.scrollHeight, document.documentElement.scrollHeight)";
    let page_height = tab.evaluate(height_js).await?.as_f64().unwrap_or(800.0) as u32;

    let final_height = page_height.min(config.max_height).max(100);

    let capture_viewport = Viewport::new(config.viewport_width, final_height)
        .with_device_scale_factor(config.device_scale_factor);

    tab.set_viewport(&capture_viewport).await?;

    if page_height > 800 {
        time::sleep(Duration::from_millis(500)).await;
    }

    let format = if config.quality >= 100 {
        ImageFormat::Png
    } else {
        ImageFormat::Jpeg
    };

    let opts = CaptureOptions::new()
        .with_viewport(capture_viewport)
        .with_format(format)
        .with_quality(config.quality)
        .with_full_page(true);

    let base64_data = tab.screenshot(opts).await;
    let _ = tab.close().await;

    base64_data.map_err(|e| anyhow!("Screenshot failed: {}", e))
}

// ================= Utils =================

static URL_REGEX: OnceLock<Regex> = OnceLock::new();

fn extract_url(text: &str) -> Option<String> {
    let re = URL_REGEX
        .get_or_init(|| Regex::new(r"https?://[^\s\u4e00-\u9fa5]+").expect("Invalid Regex"));
    re.find(text).map(|m| m.as_str().to_string())
}

fn should_process(group_id: Option<i64>, white: &[i64], black: &[i64]) -> bool {
    let gid = match group_id {
        Some(id) => id,
        None => return true, // 私聊默认处理
    };

    if black.contains(&gid) {
        return false;
    }

    if !white.is_empty() && !white.contains(&gid) {
        return false;
    }

    true
}

// ================= Main Handler =================

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        // 尝试解析为消息事件
        let msg_event = match ctx.as_message() {
            Some(e) => e,
            None => return Ok(Some(ctx)),
        };

        // 读取配置
        let config: Config = get_config(&ctx, "webshot").unwrap_or_else(|| {
            // 这里的 fallback 主要是为了防止反序列化失败，正常情况下 init 会写入默认配置
            let val = default_config();
            serde_json::from_value(serde_json::to_value(val).unwrap()).unwrap()
        });

        // 获取全局浏览器路径配置
        let browser_path = ctx.config.read().unwrap().browser_path.clone();

        // 检查群组黑白名单
        let group_id = msg_event.group_id();
        if !should_process(group_id, &config.channel.white, &config.channel.black) {
            return Ok(Some(ctx));
        }

        let user_id = msg_event.user_id();
        let self_id = ctx.bot.login_user.id.parse::<i64>().unwrap_or(0);

        if user_id == self_id {
            return Ok(Some(ctx));
        }

        // 提取 URL
        let url_candidate = if let crate::event::EventType::Onebot(event) = &ctx.event {
            if let Some(arr) = event.get_array("message") {
                arr.iter()
                    .filter(|seg| seg.get_str("type") == Some("text"))
                    .find_map(|seg| {
                        seg.get("data")
                            .and_then(|d| d.get_str("text"))
                            .and_then(extract_url)
                    })
            } else {
                extract_url(msg_event.text())
            }
        } else {
            extract_url(msg_event.text())
        };

        if let Some(url) = url_candidate {
            // 检查域名屏蔽
            for ignore in &config.ignore_domains {
                if url.contains(ignore) {
                    return Ok(Some(ctx));
                }
            }

            // 执行截图
            info!(target: "WebShot", "Capturing: {}", url);

            match capture_url(&url, &config, browser_path).await {
                Ok(base64_img) => {
                    let msg = Message::new()
                        .reply(msg_event.message_id())
                        .image(format!("base64://{}", base64_img));

                    send_msg(&ctx, writer, group_id, Some(user_id), msg).await?;
                }
                Err(e) => {
                    error!(target: "WebShot", "Error capturing {}: {}", url, e);
                }
            }
        }

        Ok(Some(ctx))
    })
}
