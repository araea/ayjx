#![allow(dead_code)]

use crate::adapters::satori::{LockedWriter, api};
use crate::event::Context;
use regex::Regex;
use simd_json::OwnedValue;
use simd_json::base::ValueAsScalar;
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::sync::OnceLock;

pub struct CommandMatch {
    /// 匹配后的参数列表（剩余的消息段）
    pub args: Vec<OwnedValue>,
    /// 被过滤掉的引用回复 ID
    pub reply_id: Option<String>,
    /// 被过滤掉的 AT 用户 ID 列表
    pub at_ids: Vec<String>,
}

pub fn get_prefixes(ctx: &Context) -> Vec<String> {
    ctx.config.read().unwrap().command_prefix.clone()
}

/// 在多条候选指令中返回第一个命中的匹配
pub fn first_command_match(ctx: &Context, commands: &[&str]) -> Option<CommandMatch> {
    commands.iter().find_map(|cmd| match_command(ctx, cmd))
}

/// 把 CommandMatch 的参数拼接为纯文本
///
/// 段与段之间补一个空格，避免多段文本参数粘连成一个词。
pub fn extract_text_arg(args: &[OwnedValue]) -> String {
    let mut buf = String::new();
    for seg in args {
        if seg.get_str("type") == Some("text")
            && let Some(text) = seg.get("data").and_then(|d| d.get_str("text"))
        {
            buf.push_str(text);
            buf.push(' ');
        }
    }
    buf.trim().to_string()
}

/// 剥离消息前缀：配置了前缀则必须命中其一，未配置前缀则原样放行
pub fn strip_prefix<'a>(ctx: &Context, text: &'a str) -> Option<&'a str> {
    let text = text.trim();
    let prefixes = get_prefixes(ctx);
    if prefixes.is_empty() {
        return Some(text);
    }
    prefixes
        .iter()
        .find_map(|p| text.strip_prefix(p.as_str()).map(|rest| rest.trim_start()))
}

/// 提取文本中第一个 http(s) URL（自动排除中文粘连）
pub fn find_url(text: &str) -> Option<String> {
    static URL_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = URL_REGEX
        .get_or_init(|| Regex::new(r"https?://[^\s\u4e00-\u9fa5]+").expect("Invalid Regex"));
    re.find(text).map(|m| m.as_str().to_string())
}

/// 从指令参数或引用回复中提取第一张图片的 URL
pub async fn get_image_url(
    ctx: &Context,
    writer: LockedWriter,
    args: &[OwnedValue],
    reply_id: Option<&String>,
) -> Option<String> {
    // 1. 指令参数中直接携带图片
    for seg in args {
        if seg.get_str("type") == Some("image")
            && let Some(data) = seg.get("data")
            && let Some(url) = data.get_str("url")
        {
            return Some(url.to_string());
        }
    }

    // 2. 引用回复中的图片
    let rid = reply_id?.parse::<i64>().ok()?;
    let resp = api::get_msg(ctx, writer, rid).await.ok()?;
    resp.message.0.iter().find_map(|seg| {
        if seg.type_ == "image"
            && let Some(url) = seg.data.get("url").and_then(|v| v.as_str())
        {
            Some(url.to_string())
        } else {
            None
        }
    })
}

/// 解析指令：自动过滤头部的 Reply/At/空白，匹配 [Prefix][Command]，返回参数及引用信息
pub fn match_command(ctx: &Context, command_name: &str) -> Option<CommandMatch> {
    let prefixes = get_prefixes(ctx);
    // 仅处理 MessageEvent
    let msg_arr = ctx.as_message()?.0.get_array("message")?;

    let mut reply_id = None;
    let mut at_ids = Vec::new();

    for (i, segment) in msg_arr.iter().enumerate() {
        let type_ = segment.get_str("type")?;
        let data = segment.get("data")?;

        match type_ {
            "reply" => {
                if reply_id.is_none() {
                    // 尝试获取 id (可能是字符串或数字)
                    let id_str = data
                        .get_str("id")
                        .map(String::from)
                        .or_else(|| data.get_i64("id").map(|v| v.to_string()))
                        .or_else(|| data.get_u64("id").map(|v| v.to_string()));
                    reply_id = id_str;
                }
            }
            "at" => {
                let qq_str = data
                    .get_str("qq")
                    .map(String::from)
                    .or_else(|| data.get_i64("qq").map(|v| v.to_string()))
                    .or_else(|| data.get_u64("qq").map(|v| v.to_string()));
                if let Some(qq) = qq_str {
                    at_ids.push(qq);
                }
            }
            "text" => {
                let raw_text = data.get_str("text").unwrap_or("");
                // 跳过首部纯空白文本
                let trimmed_start = raw_text.trim_start();
                if trimmed_start.is_empty() {
                    continue;
                }

                // 找到第一个有效文本节点，尝试匹配
                for prefix in &prefixes {
                    let target = format!("{}{}", prefix, command_name);
                    if trimmed_start.starts_with(&target) {
                        // 匹配成功
                        let mut args = Vec::new();

                        // 处理当前文本节点剩余部分
                        let rest_of_text = &trimmed_start[target.len()..];
                        // 指令后通常有空格，作为参数时去除左侧空格
                        let args_text = rest_of_text.trim_start();

                        if !args_text.is_empty() {
                            let mut new_seg = segment.clone();
                            new_seg["data"]["text"] = OwnedValue::from(args_text);
                            args.push(new_seg);
                        }

                        // 将后续所有节点加入 args
                        for seg in msg_arr.iter().skip(i + 1) {
                            args.push(seg.clone());
                        }

                        return Some(CommandMatch {
                            reply_id,
                            at_ids,
                            args,
                        });
                    }
                }
                // 如果遇到第一个有效文本但未匹配成功，则视为匹配失败
                return None;
            }
            // 遇到其他类型（如图片）且未匹配到指令，停止
            _ => return None,
        }
    }

    None
}
