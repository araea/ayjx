use super::data::Manager;
use super::logic::{reply_text, to_data_url};
use super::types::{Agent, MjMessageTask};
use super::utils::normalize;
use crate::adapters::satori::{LockedWriter, api, send_msg, send_msg_id};
use crate::event::Context;
use crate::message::Message;
use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const MJ_MODELS: &[&str] = &["mj", "mj-describe", "mj-shorten", "mj-blend"];

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const TASK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_IMAGE_BYTES: usize = 30 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MjMode {
    Imagine,
    Describe,
    Shorten,
    Blend,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct MjButton {
    #[serde(deserialize_with = "null_default")]
    custom_id: String,
    #[serde(deserialize_with = "null_default")]
    label: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct MjTask {
    #[serde(deserialize_with = "null_default")]
    id: String,
    #[serde(deserialize_with = "null_default")]
    action: String,
    #[serde(deserialize_with = "null_default")]
    status: String,
    #[serde(deserialize_with = "null_default")]
    progress: String,
    #[serde(deserialize_with = "null_default")]
    description: String,
    #[serde(deserialize_with = "null_default")]
    fail_reason: String,
    #[serde(deserialize_with = "null_default")]
    image_url: String,
    #[serde(deserialize_with = "null_default")]
    prompt: String,
    #[serde(deserialize_with = "null_default")]
    prompt_en: String,
    properties: Value,
    #[serde(deserialize_with = "null_default")]
    buttons: Vec<MjButton>,
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn mode(model: &str) -> Option<MjMode> {
    match model.trim().to_ascii_lowercase().as_str() {
        "mj" => Some(MjMode::Imagine),
        "mj-describe" => Some(MjMode::Describe),
        "mj-shorten" => Some(MjMode::Shorten),
        "mj-blend" => Some(MjMode::Blend),
        _ => None,
    }
}

pub fn is_mj_model(model: &str) -> bool {
    mode(model).is_some()
}

/// OpenAI 接口通常配置为 `.../v1`，MJ 则从同一服务根路径的 fast 接入点调用。
/// 如果管理员已经显式填了 `mj-fast` / `mj-relax`，尊重该选择。
fn api_base(configured: &str) -> String {
    let mut base = configured.trim().trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/v1") {
        base = stripped.trim_end_matches('/');
    }
    if base.ends_with("/mj-fast")
        || base.ends_with("/mj-relax")
        || base.ends_with("/fast")
        || base.ends_with("/relax")
    {
        base.to_string()
    } else {
        format!("{base}/mj-fast")
    }
}

fn api_bases(configured: &str) -> Vec<String> {
    let primary = api_base(configured);
    if primary.ends_with("/mj-fast") {
        vec![primary.clone(), primary.trim_end_matches("/mj-fast").to_string() + "/mj-relax"]
    } else if primary.ends_with("/fast") {
        vec![primary.clone(), primary.trim_end_matches("/fast").to_string() + "/relax"]
    } else {
        vec![primary]
    }
}

pub async fn handle_agent(
    agent: &Agent,
    user_prompt: &str,
    images: Vec<String>,
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
) {
    let event = match ctx.as_message() {
        Some(event) => event,
        None => return,
    };
    let Some(mode) = mode(&agent.model) else {
        return;
    };
    let (configured_base, key) = {
        let config = mgr.config.read().await;
        (config.api_base.clone(), config.api_key.clone())
    };
    if configured_base.trim().is_empty() || key.trim().is_empty() {
        reply_text(ctx, writer, &event, "❌ API 未配置。").await;
        return;
    }

    let prompt = join_prompt(&agent.system_prompt, user_prompt);
    let validation_error = match mode {
        MjMode::Imagine if prompt.is_empty() => Some("💬 请输入绘图提示词。"),
        MjMode::Describe if images.is_empty() => Some("🖼️ 请随消息发送或引用一张图片。"),
        MjMode::Shorten if user_prompt.trim().is_empty() => Some("💬 请输入要精简的提示词。"),
        MjMode::Blend if images.len() < 2 => Some("🖼️ Blend 至少需要两张图片，可直接发送或引用图片。"),
        _ => None,
    };
    if let Some(message) = validation_error {
        reply_text(ctx, writer, &event, message).await;
        return;
    }

    let annotate = !event.is_manual_self();
    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, true).await;
    }
    let bases = api_bases(&configured_base);
    let result = async {
        let body = match mode {
            MjMode::Imagine => json!({
                "base64Array": image_data_urls(&images).await,
                "prompt": prompt,
            }),
            MjMode::Describe => json!({
                "base64": to_data_url(&images[0]).await,
            }),
            MjMode::Shorten => json!({ "prompt": user_prompt.trim() }),
            MjMode::Blend => json!({
                "base64Array": image_data_urls(&images[..images.len().min(5)]).await,
                "dimensions": blend_dimensions(user_prompt),
            }),
        };
        let endpoint = match mode {
            MjMode::Imagine => "/mj/submit/imagine",
            MjMode::Describe => "/mj/submit/describe",
            MjMode::Shorten => "/mj/submit/shorten",
            MjMode::Blend => "/mj/submit/blend",
        };
        let (base, task_id) = submit_with_fallback(&bases, &key, endpoint, body).await?;
        let task = poll(&base, &key, &task_id).await?;
        Ok::<_, anyhow::Error>((base, task))
    }
    .await;

    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, false).await;
    }
    match result {
        Ok((base, task)) => {
            deliver_task(ctx, writer, mgr, mode, &base, &task, event.message_id()).await
        }
        Err(error) => {
            warn!(target: "Plugin/OAI/MJ", "MJ {} 任务失败: {:#}", agent.model, error);
            reply_text(ctx, writer, &event, format!("❌ MJ 任务失败：{error}")).await;
        }
    }
}

fn join_prompt(system: &str, user: &str) -> String {
    match (system.trim(), user.trim()) {
        ("", user) => user.to_string(),
        (system, "") => system.to_string(),
        (system, user) => format!("{system}\n{user}"),
    }
}

async fn image_data_urls(images: &[String]) -> Vec<String> {
    // 图片通常来自同一条消息，数量很少；并行下载能明显减少垫图等待。
    futures_util::future::join_all(images.iter().map(|url| to_data_url(url))).await
}

fn blend_dimensions(prompt: &str) -> &'static str {
    let lower = normalize(prompt).to_ascii_lowercase();
    if lower.contains("portrait") || lower.contains("竖") || lower.contains("2:3") {
        "PORTRAIT"
    } else if lower.contains("landscape") || lower.contains("横") || lower.contains("3:2") {
        "LANDSCAPE"
    } else {
        "SQUARE"
    }
}

async fn submit(base: &str, key: &str, endpoint: &str, body: Value) -> anyhow::Result<String> {
    let response = crate::http::client()
        .post(format!("{base}{endpoint}"))
        .bearer_auth(key)
        .json(&body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .with_context(|| format!("提交 {endpoint} 请求失败"))?;
    let status = response.status();
    let bytes = response.bytes().await.context("读取提交响应失败")?;
    if !status.is_success() {
        return Err(anyhow!(
            "提交接口返回 HTTP {}：{}",
            status.as_u16(),
            response_excerpt(&bytes)
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).context("提交响应不是有效 JSON")?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or_default();
    if code != 1 && code != 22 {
        return Err(anyhow!(
            "{}",
            value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("任务提交失败")
        ));
    }
    scalar_string(value.get("result")).filter(|id| !id.is_empty()).ok_or_else(|| anyhow!("提交成功但未返回任务 ID"))
}

async fn submit_with_fallback(
    bases: &[String],
    key: &str,
    endpoint: &str,
    body: Value,
) -> anyhow::Result<(String, String)> {
    let mut last_error = None;
    for (position, base) in bases.iter().enumerate() {
        match submit(base, key, endpoint, body.clone()).await {
            Ok(task_id) => return Ok((base.clone(), task_id)),
            Err(error) => {
                let retryable = error.to_string().contains("HTTP 503")
                    || error.to_string().contains("无可用渠道")
                    || error.to_string().contains("HTTP 404");
                if !retryable || position + 1 == bases.len() {
                    return Err(error);
                }
                warn!(target: "Plugin/OAI/MJ", "MJ 接入点 {} 暂不可用，尝试备用模式: {}", base, error);
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的 MJ 接入点")))
}

async fn poll(base: &str, key: &str, task_id: &str) -> anyhow::Result<MjTask> {
    let deadline = tokio::time::Instant::now() + TASK_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("等待任务 {task_id} 超时"));
        }
        let response = crate::http::client()
            .get(format!("{base}/mj/task/{task_id}/fetch"))
            .bearer_auth(key)
            .timeout(Duration::from_secs(45))
            .send()
            .await
            .context("查询任务失败")?;
        let status = response.status();
        let bytes = response.bytes().await.context("读取任务响应失败")?;
        if !status.is_success() {
            return Err(anyhow!(
                "查询接口返回 HTTP {}：{}",
                status.as_u16(),
                response_excerpt(&bytes)
            ));
        }
        let raw: Value = serde_json::from_slice(&bytes).context("任务响应不是有效 JSON")?;
        if raw.get("code").is_some() && raw.get("status").is_none() {
            return Err(anyhow!(
                "{}",
                raw.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("任务查询失败")
            ));
        }
        let task: MjTask = serde_json::from_value(raw).context("无法解析任务响应")?;
        match task.status.to_ascii_uppercase().as_str() {
            "SUCCESS" => return Ok(task),
            "FAILURE" => {
                let reason = if task.fail_reason.trim().is_empty() {
                    task.description
                } else {
                    task.fail_reason
                };
                return Err(anyhow!(if reason.is_empty() {
                    "MJ 任务执行失败".to_string()
                } else {
                    reason
                }));
            }
            _ if task.progress.trim() == "100%" => return Ok(task),
            _ => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

async fn deliver_task(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
    mode: MjMode,
    api_base: &str,
    task: &MjTask,
    reply_to: i64,
) {
    let event = match ctx.as_message() {
        Some(event) => event,
        None => return,
    };
    if matches!(mode, MjMode::Imagine | MjMode::Blend) && !task.image_url.trim().is_empty() {
        let image = image_payload(task.image_url.trim()).await;
        let message = Message::new()
            .reply(reply_to)
            .image(image);
        match send_msg_id(
            ctx,
            writer.clone(),
            event.group_id(),
            Some(event.user_id()),
            message,
        )
        .await
        {
            Ok(Some(message_id)) => remember_grid(mgr, message_id, api_base, task).await,
            Ok(None) => warn!(target: "Plugin/OAI/MJ", "绘图已发送，但实现端未返回消息 ID，无法关联后续放大"),
            Err(error) => warn!(target: "Plugin/OAI/MJ", "发送 MJ 图片失败: {error}"),
        }
        return;
    }

    let text = task_text(task);
    reply_text(
        ctx,
        writer,
        &event,
        if text.is_empty() {
            "✅ MJ 任务已完成。".to_string()
        } else {
            text
        },
    )
    .await;
}

fn task_text(task: &MjTask) -> String {
    for key in ["messageContent", "result", "content", "finalPrompt"] {
        if let Some(value) = task.properties.get(key).and_then(Value::as_str)
            && !value.trim().is_empty()
        {
            return value.trim().to_string();
        }
    }
    let description = task.description.trim();
    if !description.is_empty()
        && !description.eq_ignore_ascii_case("submit success")
        && description != "提交成功"
    {
        return description.to_string();
    }
    if !task.prompt_en.trim().is_empty() {
        task.prompt_en.trim().to_string()
    } else {
        task.prompt.trim().to_string()
    }
}

async fn remember_grid(mgr: &Arc<Manager>, message_id: String, api_base: &str, task: &MjTask) {
    let upscale_buttons = task
        .buttons
        .iter()
        .filter_map(|button| {
            let label = button.label.trim().to_ascii_uppercase();
            let index = label.strip_prefix('U')?.parse::<u8>().ok()?;
            (1..=4)
                .contains(&index)
                .then(|| (index, button.custom_id.clone()))
        })
        .collect::<HashMap<_, _>>();
    if upscale_buttons.is_empty() {
        return;
    }
    let mut cache = mgr.mj_cache.write().await;
    cache.messages.insert(
        message_id,
        MjMessageTask {
            task_id: task.id.clone(),
            api_base: api_base.to_string(),
            upscale_buttons,
            created_at: chrono::Local::now().timestamp(),
        },
    );
    mgr.save_mj_cache(&cache);
}

pub async fn try_handle_upscale_reply(
    ctx: &Context,
    writer: &LockedWriter,
    mgr: &Arc<Manager>,
) -> bool {
    let Some(reply_id) = reply_message_id(ctx) else {
        return false;
    };
    let indices = upscale_indices(ctx);
    if indices.is_empty() {
        return false;
    }
    let source = {
        let cache = mgr.mj_cache.read().await;
        cache.messages.get(&reply_id).cloned()
    };
    let Some(source) = source else {
        return false;
    };
    let event = match ctx.as_message() {
        Some(event) => event,
        None => return false,
    };
    let (configured_base, key) = {
        let config = mgr.config.read().await;
        (config.api_base.clone(), config.api_key.clone())
    };
    if configured_base.trim().is_empty() || key.trim().is_empty() {
        reply_text(ctx, writer, &event, "❌ API 未配置。").await;
        return true;
    }
    let bases = if source.api_base.is_empty() {
        api_bases(&configured_base)
    } else {
        vec![source.api_base.clone()]
    };
    let annotate = !event.is_manual_self();
    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, true).await;
    }

    for index in indices {
        let cache_key = format!("{}:{index}", source.task_id);
        if let Some(cached) = {
            let cache = mgr.mj_cache.read().await;
            cache.upscales.get(&cache_key).cloned()
        } {
            if send_cached_image(ctx, writer, &cached, event.message_id()).await {
                continue;
            }
            let mut cache = mgr.mj_cache.write().await;
            cache.upscales.remove(&cache_key);
            mgr.save_mj_cache(&cache);
        }

        let Some(custom_id) = source.upscale_buttons.get(&index) else {
            reply_text(ctx, writer, &event, format!("❌ 这张图没有可用的 U{index} 放大操作。"))
                .await;
            continue;
        };
        {
            let mut inflight = mgr.mj_inflight.write().await;
            if !inflight.insert(cache_key.clone()) {
                reply_text(ctx, writer, &event, format!("⏳ 第 {index} 张正在放大，请稍候。"))
                    .await;
                continue;
            }
        }

        let result = async {
            let (base, task_id) = submit_with_fallback(
                &bases,
                &key,
                "/mj/submit/action",
                json!({ "customId": custom_id, "taskId": source.task_id }),
            )
            .await?;
            poll(&base, &key, &task_id).await
        }
        .await;
        mgr.mj_inflight.write().await.remove(&cache_key);

        match result {
            Ok(task) if !task.image_url.trim().is_empty() => {
                let cached = cache_upscale_image(mgr, &cache_key, task.image_url.trim()).await;
                {
                    let mut cache = mgr.mj_cache.write().await;
                    cache.upscales.insert(cache_key, cached.clone());
                    mgr.save_mj_cache(&cache);
                }
                if !send_cached_image(ctx, writer, &cached, event.message_id()).await {
                    reply_text(ctx, writer, &event, "❌ 放大完成，但结果图片发送失败。").await;
                }
            }
            Ok(_) => reply_text(ctx, writer, &event, "❌ 放大完成，但接口未返回图片。").await,
            Err(error) => {
                warn!(target: "Plugin/OAI/MJ", "MJ U{} 放大失败: {:#}", index, error);
                reply_text(ctx, writer, &event, format!("❌ 第 {index} 张放大失败：{error}"))
                    .await;
            }
        }
    }
    if annotate {
        let _ = api::set_msg_emoji_like(ctx, writer.clone(), event.message_id(), 124, false).await;
    }
    true
}

fn reply_message_id(ctx: &Context) -> Option<String> {
    let crate::event::EventType::Satori(event) = &ctx.event else {
        return None;
    };
    let segments = event.get_array("message")?;
    let data = segments
        .iter()
        .find(|segment| segment.get_str("type") == Some("reply"))?
        .get("data")?;
    data.get_str("id")
        .map(str::to_string)
        .or_else(|| data.get_i64("id").map(|value| value.to_string()))
        .or_else(|| data.get_u64("id").map(|value| value.to_string()))
}

fn upscale_indices(ctx: &Context) -> Vec<u8> {
    let crate::event::EventType::Satori(event) = &ctx.event else {
        return Vec::new();
    };
    let Some(segments) = event.get_array("message") else {
        return Vec::new();
    };
    indices_from_texts(
        segments
        .iter()
        .filter(|segment| segment.get_str("type") == Some("text"))
            .filter_map(|segment| segment.get("data")?.get_str("text")),
    )
}

fn indices_from_texts<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let mut selected = HashSet::new();
    for text in texts {
        for character in normalize(text).chars() {
            if let Some(index) = character.to_digit(10).map(|value| value as u8)
                && (1..=4).contains(&index)
            {
                selected.insert(index);
            }
        }
    }
    let mut selected = selected.into_iter().collect::<Vec<_>>();
    selected.sort_unstable();
    selected
}

async fn cache_upscale_image(mgr: &Arc<Manager>, key: &str, url: &str) -> String {
    match download_image(url).await {
        Ok(bytes) => {
            let safe_key = key
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>();
            let path = mgr.mj_images_dir.join(format!("{safe_key}.img"));
            match tokio::fs::write(&path, bytes).await {
                Ok(()) => path.to_string_lossy().into_owned(),
                Err(error) => {
                    warn!(target: "Plugin/OAI/MJ", "写入放大缓存失败: {error}");
                    url.to_string()
                }
            }
        }
        Err(error) => {
            warn!(target: "Plugin/OAI/MJ", "下载放大结果用于缓存失败: {error:#}");
            url.to_string()
        }
    }
}

async fn download_image(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = crate::http::client()
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/122 Safari/537.36",
        )
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .context("下载图片失败")?;
    if !response.status().is_success() {
        return Err(anyhow!("图片服务器返回 HTTP {}", response.status().as_u16()));
    }
    let bytes = response.bytes().await.context("读取图片失败")?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(anyhow!("图片为空或超过 30 MiB"));
    }
    Ok(bytes.to_vec())
}

async fn image_payload(url: &str) -> String {
    match download_image(url).await {
        Ok(bytes) => format!(
            "base64://{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        Err(error) => {
            warn!(target: "Plugin/OAI/MJ", "内联 MJ 结果图片失败，回退远程地址: {error:#}");
            url.to_string()
        }
    }
}

async fn send_cached_image(
    ctx: &Context,
    writer: &LockedWriter,
    source: &str,
    reply_to: i64,
) -> bool {
    let event = match ctx.as_message() {
        Some(event) => event,
        None => return false,
    };
    let image = if Path::new(source).is_file() {
        match tokio::fs::read(source).await {
            Ok(bytes) if !bytes.is_empty() => format!(
                "base64://{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
            _ => return false,
        }
    } else {
        source.to_string()
    };
    send_msg(
        ctx,
        writer.clone(),
        event.group_id(),
        Some(event.user_id()),
        Message::new().reply(reply_to).image(image),
    )
    .await
    .is_ok()
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(value) = value.as_str() {
        Some(value.to_string())
    } else if let Some(value) = value.as_i64() {
        Some(value.to_string())
    } else {
        value.as_u64().map(|value| value.to_string())
    }
}

fn response_excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(500)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_explicit_mj_room_models() {
        for model in MJ_MODELS {
            assert!(is_mj_model(model));
        }
        assert!(is_mj_model(" MJ "));
        assert!(!is_mj_model("mj-v6"));
        assert!(!is_mj_model("not-mj"));
    }

    #[test]
    fn derives_mj_endpoint_from_openai_endpoint() {
        assert_eq!(
            api_base("https://api.apilio.ai/v1/"),
            "https://api.apilio.ai/mj-fast"
        );
        assert_eq!(
            api_base("https://api.apilio.ai/mj-relax"),
            "https://api.apilio.ai/mj-relax"
        );
        assert_eq!(
            api_bases("https://api.apilio.ai/v1"),
            vec![
                "https://api.apilio.ai/mj-fast",
                "https://api.apilio.ai/mj-relax"
            ]
        );
    }

    #[test]
    fn understands_convenient_blend_dimensions() {
        assert_eq!(blend_dimensions("横图"), "LANDSCAPE");
        assert_eq!(blend_dimensions("portrait"), "PORTRAIT");
        assert_eq!(blend_dimensions("随便融合"), "SQUARE");
    }

    #[test]
    fn collects_and_deduplicates_all_requested_upscales() {
        assert_eq!(
            indices_from_texts(["请放大４、2", "再要 2 和 1，忽略 9"].into_iter()),
            vec![1, 2, 4]
        );
    }

    #[test]
    fn chooses_useful_text_from_task() {
        let task = MjTask {
            description: "Submit success".to_string(),
            prompt: "original".to_string(),
            properties: json!({"messageContent": "one\ntwo"}),
            ..Default::default()
        };
        assert_eq!(task_text(&task), "one\ntwo");
    }

    #[test]
    fn accepts_null_buttons_and_nullable_task_fields() {
        let task: MjTask = serde_json::from_value(json!({
            "id": "task-1",
            "status": "IN_PROGRESS",
            "buttons": null,
            "imageUrl": null,
            "failReason": null
        }))
        .unwrap();
        assert!(task.buttons.is_empty());
        assert!(task.image_url.is_empty());
        assert!(task.fail_reason.is_empty());
    }
}
