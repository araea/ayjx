use super::types::{DecideResult, PersonaConfig, ReplyResult};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequestArgs,
    },
};
use std::time::Duration;

fn make_client(cfg: &PersonaConfig) -> Client<OpenAIConfig> {
    let oc = OpenAIConfig::new()
        .with_api_base(cfg.api_base.clone())
        .with_api_key(cfg.api_key.clone());
    Client::with_config(oc)
}

fn build_messages(system: &str, user: &str) -> Vec<ChatCompletionRequestMessage> {
    let mut v: Vec<ChatCompletionRequestMessage> = Vec::with_capacity(2);
    if let Ok(m) = ChatCompletionRequestSystemMessageArgs::default()
        .content(system.to_string())
        .build()
    {
        v.push(m.into());
    }
    if let Ok(m) = ChatCompletionRequestUserMessageArgs::default()
        .content(user.to_string())
        .build()
    {
        v.push(m.into());
    }
    v
}

fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0
                    && let Some(st) = start
                {
                    return std::str::from_utf8(&bytes[st..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

pub async fn decide(cfg: &PersonaConfig, system: &str, user: &str) -> anyhow::Result<DecideResult> {
    let client = make_client(cfg);
    let req = CreateChatCompletionRequestArgs::default()
        .model(&cfg.decide_model)
        .messages(build_messages(system, user))
        .max_tokens(120u32)
        .temperature(0.4)
        .build()?;

    let res = tokio::time::timeout(Duration::from_secs(20), client.chat().create(req))
        .await
        .map_err(|_| anyhow::anyhow!("decide timeout"))??;

    let raw = res
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    let json_str = extract_json_object(&raw).ok_or_else(|| anyhow::anyhow!("no json in decide"))?;
    let parsed: DecideResult = serde_json::from_str(json_str)?;
    Ok(parsed)
}

pub async fn generate(
    cfg: &PersonaConfig,
    system: &str,
    user: &str,
) -> anyhow::Result<ReplyResult> {
    let client = make_client(cfg);
    let req = CreateChatCompletionRequestArgs::default()
        .model(&cfg.reply_model)
        .messages(build_messages(system, user))
        .max_tokens(360u32)
        .temperature(0.85)
        .build()?;

    let res = tokio::time::timeout(Duration::from_secs(60), client.chat().create(req))
        .await
        .map_err(|_| anyhow::anyhow!("reply timeout"))??;

    let raw = res
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();

    if let Some(json_str) = extract_json_object(&raw)
        && let Ok(parsed) = serde_json::from_str::<ReplyResult>(json_str)
    {
        return Ok(parsed);
    }

    // 兜底：当成单条文本返回
    let cleaned = raw.trim().trim_matches('"').to_string();
    if cleaned.is_empty() {
        return Err(anyhow::anyhow!("empty reply"));
    }
    Ok(ReplyResult {
        messages: vec![cleaned],
        ..Default::default()
    })
}
