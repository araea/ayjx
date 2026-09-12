//! OpenAI 兼容补全的唯一入口。
//!
//! 普通房间与判定都从这里发一轮请求：接口地址、密钥、模型 id 由调用方解析好，
//! 这里只负责拼请求、发出去、把候选回复里的文本取出来。
//!
//! 之前这层用 `async-openai`，两个痛点：它的强类型反序列化对「字段不全」的
//! 中转站响应过于挑剔（模型列表尤其明显），而本项目真正需要的只有
//! 「chat/completions 一发一收」这一条。`rig-core` 的请求结构是公开的普通结构体，
//! 想加什么参数直接写进 `additional_params` 即可（provider 会把它扁平进顶层 body），
//! 不必为每个参数准备一个 builder。

use rig_core::client::{BearerAuth, ClientBuilder, CompletionClient};
use rig_core::completion::{AssistantContent, CompletionModel, CompletionRequest, Message};
use rig_core::providers::openai::{CompletionsClient, OpenAICompletionsExtBuilder};

/// 按接口地址与密钥建一个 Chat Completions 客户端。
///
/// 复用进程级的 `crate::http::client()`：它与其它出网请求共用连接池与代理设置，
/// 在手机上少开一套 TLS 栈。
pub(crate) fn client(api_base: &str, api_key: &str) -> anyhow::Result<CompletionsClient> {
    ClientBuilder::<OpenAICompletionsExtBuilder>::default()
        .api_key::<BearerAuth>(api_key.to_string())
        .base_url(super::utils::openai_api_base(api_base))
        .http_client(crate::http::client())
        .build()
        .map_err(|error| anyhow::anyhow!("构建 OpenAI 客户端失败：{error}"))
}

/// 把房间的思考强度映射成 Chat Completions 的 `reasoning_effort`。
///
/// 档位沿用房间的写法；`off` 是「不思考」，等价于 OpenAI 的 `none`。
fn reasoning_effort(level: &str) -> Option<&'static str> {
    Some(match level.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        _ => return None,
    })
}

/// 组装一次补全请求。
///
/// `tools` 为空就是普通房间的形态——请求里连 tools 字段都不会有意义；
/// 思考强度走 `additional_params.reasoning_effort`，由 provider 扁平进顶层。
/// `temperature` 为 `None` 时连这个字段都不发，交给接口自己的默认值。
pub(crate) fn request(
    history: Vec<Message>,
    tools: Vec<rig_core::completion::ToolDefinition>,
    thinking: Option<&str>,
    temperature: Option<f64>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: history,
        documents: Vec::new(),
        tools,
        temperature,
        max_tokens: None,
        tool_choice: None,
        additional_params: thinking
            .and_then(reasoning_effort)
            .map(|effort| serde_json::json!({ "reasoning_effort": effort })),
        output_schema: None,
        record_telemetry_content: false,
    }
}

/// 取出一份回复里的全部正文。
pub(crate) fn text_of(choice: &[AssistantContent]) -> String {    choice
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// 单轮补全：无工具、无历史维护，返回候选回复的正文。
///
/// 普通房间与群聊判定都走这里；带工具的多轮循环在 [`super::agent`]。
pub(crate) async fn complete(
    api_base: &str,
    api_key: &str,
    model: &str,
    history: Vec<Message>,
    thinking: Option<&str>,
) -> anyhow::Result<String> {
    let response = client(api_base, api_key)?
        .completion_model(model)
        .completion(request(history, Vec::new(), thinking, None))
        .await?;
    let text = text_of(&response.choice);
    if text.trim().is_empty() {
        anyhow::bail!("API 返回了空回复");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{
        Reasoning, Text, ToolCall, ToolCallId, ToolFunction, UserContent,
    };

    fn hello() -> Vec<Message> {
        vec![Message::User {
            content: vec![UserContent::Text(Text::new("你好"))],
        }]
    }

    /// 普通房间的请求里没有工具；思考强度写进 `additional_params` 等 provider 扁平化。
    #[test]
    fn ordinary_requests_carry_no_tools() {
        let request = request(hello(), Vec::new(), None, None);
        assert!(request.tools.is_empty());
        assert!(request.additional_params.is_none());
        // 没配温度就连这个字段都不发，交给接口自己的默认值。
        assert!(request.temperature.is_none());
    }

    /// 群聊那边配了温度就得原样带上，判定与普通房间不受影响。
    #[test]
    fn a_configured_temperature_reaches_the_request() {
        assert_eq!(
            request(hello(), Vec::new(), Some("low"), Some(1.3)).temperature,
            Some(1.3)
        );
    }

    #[test]
    fn thinking_becomes_reasoning_effort() {
        for (level, expected) in [
            ("high", "high"),
            ("off", "none"),
            ("none", "none"),
            ("xhigh", "xhigh"),
            (" minimal ", "minimal"),
        ] {
            let request = request(hello(), Vec::new(), Some(level), None);
            assert_eq!(
                request.additional_params,
                Some(serde_json::json!({ "reasoning_effort": expected })),
                "{level}"
            );
        }
        // 非法档位不写进请求，交给引擎默认。
        assert!(
            request(hello(), Vec::new(), Some("unlimited"), None)
                .additional_params
                .is_none()
        );
    }

    /// 接口地址只填裸域名时补 `/v1`，脚本里配的完整路径原样保留。
    #[test]
    fn client_accepts_bare_and_pathful_bases() {
        for base in ["https://api.deepseek.com", "https://api.deepseek.com/v1"] {
            assert!(client(base, "sk-test").is_ok(), "{base}");
        }
    }

    #[test]
    fn only_text_blocks_are_joined() {
        let choice = vec![
            AssistantContent::Reasoning(Reasoning::new("secret")),
            AssistantContent::Text(Text::new("最终")),
            AssistantContent::Text(Text::new("回复")),
            AssistantContent::ToolCall(ToolCall::new(
                ToolCallId::mint(),
                ToolFunction::new("bash".to_string(), serde_json::json!({"command": "ls"})),
            )),
        ];
        assert_eq!(text_of(&choice), "最终回复");
    }
}
