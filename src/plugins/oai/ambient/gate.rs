//! 「这句话值不值得开口」的判定。
//!
//! 群里的绝大多数消息不需要任何回应，所以每条消息都惊动一次会写字的模型是浪费。
//! 这里先用一个便宜的多模态模型读最近的聊天记录（含图片），只回一个分数；
//! 分数过线才轮到人格模型去想说什么。判定与措辞分开，既省钱也让「沉默」这件事
//! 有一个可以被调参、被复盘的量。

use super::AmbientConfig;
use super::window::{Turn, transcript};
use super::vision;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestMessageContentPartImageArgs,
        ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs, ImageUrlArgs,
    },
};

/// 判定结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// 0-100，越高越该开口。
    pub score: u8,
    /// 一句话理由，只写进日志。
    pub reason: String,
}

/// 判定用的行为准则。
///
/// 这里不放完整人设：判定只需要知道「什么值得开口」，把整段人格塞进每条消息的
/// 判定里，既贵又会让模型开始替他写台词。
const RUBRIC: &str = "\
你在替一个惜字如金的群友判断「这段对话他想不想接一句」。他孤傲、爱较真、以反问和典故\
行事，享受在别人得意时轻轻递回一句；对蠢话连眼皮都懒得抬。但他并不是不在群里——\
群友聊得起劲的日常他也会偶尔搭一句，只是从不长篇大论。

打分（只给分，不写台词）：
- 0-15：机器人刷屏、纯表情包接龙、复读、转发链接、无人可接的碎片、与他毫无关系的私事。
- 25-40：普通的日常闲聊（吃什么、天气、游戏、手机、吐槽）。能接也能不接——
  这一档就是「偶尔搭一句」的来源，别一律压到 0。
- 55-75：话里有具体东西可接：一个观点、一处站不住的论证、一个可纠正的事实、
  一句漂亮或好笑的说法、有人抛出一个真问题、有人在自负。
- 85-100：有人直接跟他说话、@他、追问他刚说过的话，或话题正落在他在意的东西上
  （书、诗、结构漂亮的论证、人性的把戏）。
- 一律 0：色情、擦边、情感纠缠、劝架拉偏架、打听隐私、要他表态站队。

判进哪一档就给那一档的中值，别一律取下界。日常闲聊给 30 左右是正常的，
不必因为「话题不深刻」而压到 0——深不深刻由他自己决定要不要开口。

只输出 JSON，不要解释、不要代码块：{\"score\": 0-100, \"reason\": \"十五字以内\"}";

/// 读最近的聊天记录，给出开口意愿分。
pub(crate) async fn judge(
    api_base: &str,
    api_key: &str,
    config: &AmbientConfig,
    turns: &[Turn],
) -> anyhow::Result<Verdict> {
    let client = Client::with_config(
        OpenAIConfig::new()
            .with_api_base(super::super::utils::openai_api_base(api_base))
            .with_api_key(api_key),
    )
    .with_http_client(crate::http::client());

    let mut messages: Vec<ChatCompletionRequestMessage> = vec![
        ChatCompletionRequestSystemMessageArgs::default()
            .content(RUBRIC)
            .build()?
            .into(),
    ];

    let mut parts = vec![
        ChatCompletionRequestMessageContentPartTextArgs::default()
            .text(format!("最近的群聊：\n{}", transcript(turns)))
            .build()?
            .into(),
    ];
    let images = vision::usable_images(turns, config.context_images).await;
    if !images.is_empty() {
        parts.push(
            ChatCompletionRequestMessageContentPartTextArgs::default()
                .text("下面是记录里最新的图片，按出现顺序：")
                .build()?
                .into(),
        );
        for data_url in images {
            parts.push(
                ChatCompletionRequestMessageContentPartImageArgs::default()
                    .image_url(ImageUrlArgs::default().url(data_url).build()?)
                    .build()?
                    .into(),
            );
        }
    }
    messages.push(
        ChatCompletionRequestUserMessageArgs::default()
            .content(parts)
            .build()?
            .into(),
    );

    // 手机上的这条出网链路会掉连接（代理丢包、TLS 半关闭），而判定是整条搭话链路的
    // 入口：一次抖动就让这一轮群聊彻底没人看。抖动类错误重试一次，其余（如内容被
    // 模型拒收）立刻放弃——那种错误重试多少次都是同样的结果。
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = tokio::time::timeout(
            config.gate_timeout(),
            super::super::logic::complete(&client, &config.gate_model, messages.clone()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("判定超时（{} 秒）", config.gate_timeout().as_secs()))
        .and_then(|inner| inner);
        match result {
            Ok(raw) => return parse_verdict(&raw),
            Err(error) if attempt == 1 && transient(&error) => {
                debug!(target: "Plugin/OAI", "判定遇到网络抖动，重试一次：{error:#}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// 这个错误值不值得再试一次。
fn transient(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    [
        "判定超时",
        "error sending request",
        "connection",
        "timed out",
        "close_notify",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// 宽松地解析判定输出。
///
/// 模型时不时会在 JSON 外面裹一层代码块或一句「好的」，为此专门开 JSON 模式会
/// 把可用的判定模型限制在支持该参数的那几个上，不划算——取第一个花括号块即可。
fn parse_verdict(raw: &str) -> anyhow::Result<Verdict> {
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("判定模型没有返回 JSON：{}", raw.trim()))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("判定模型返回的 JSON 不完整：{}", raw.trim()))?;
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end])?;
    let score = value["score"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("判定结果缺少 score：{}", raw.trim()))?;
    Ok(Verdict {
        score: score.clamp(0.0, 100.0) as u8,
        reason: value["reason"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_flaky_transport_errors_are_worth_a_second_try() {
        assert!(transient(&anyhow::anyhow!("判定超时（45 秒）")));
        assert!(transient(&anyhow::anyhow!(
            "http error: error sending request for url (…)"
        )));
        assert!(transient(&anyhow::anyhow!("peer closed connection without close_notify")));
        // 模型拒收内容、密钥错误：重试多少次都是同一个结果。
        assert!(!transient(&anyhow::anyhow!(
            "500 Internal Server Error mime type is not supported by Gemini: 'image/gif'"
        )));
        assert!(!transient(&anyhow::anyhow!("401 Unauthorized")));
    }

    #[test]
    fn verdicts_survive_prose_and_code_fences() {
        let verdict = parse_verdict("好的\n```json\n{\"score\": 73, \"reason\": \"有错可纠\"}\n```")
            .unwrap();
        assert_eq!(
            verdict,
            Verdict {
                score: 73,
                reason: "有错可纠".into()
            }
        );
        assert_eq!(parse_verdict("{\"score\": 999}").unwrap().score, 100);
        assert!(parse_verdict("我觉得可以回复").is_err());
        assert!(parse_verdict("{\"reason\":\"x\"}").is_err());
    }

}
