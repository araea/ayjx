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
    let prefixes = ctx.config.read().unwrap().command_prefix.clone();
    if prefixes.is_empty() {
        vec![String::new()]
    } else {
        prefixes
    }
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

/// 提取文本中第一个 http(s) URL。
///
/// 群聊里的链接几乎从不独占一行：前后粘着中文，后面跟着全角逗号、句号、引号或者
/// 一对括号。「避雷这个中转站https://platform.deepseek.com，pro 模型路由到 flash」
/// 里真正的地址到 `.com` 为止，之前只排除汉字的写法会把「，pro」也算进去。
///
/// 所以这里只认 RFC 3986 允许的那些 ASCII 字符——中文、全角标点、书名号、引号
/// 都不在其中，自然断开；再把结尾那几个几乎不可能属于地址的半角标点剥掉，
/// 包括与地址内部不成对的那半个括号（`(https://example.com)` 里的右括号是外面的）。
pub fn find_url(text: &str) -> Option<String> {
    static URL_REGEX: OnceLock<Regex> = OnceLock::new();
    let re = URL_REGEX.get_or_init(|| {
        Regex::new(r"https?://[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+").expect("Invalid Regex")
    });
    let url = trim_tail(re.find(text)?.as_str());
    // 剥完之后至少还得剩个主机名，`见 https://。` 这种不算链接。
    let host = url.split_once("//").map(|(_, rest)| rest).unwrap_or("");
    (!host.is_empty()).then(|| url.to_string())
}

/// 去掉结尾那些属于句子而不属于地址的标点。
fn trim_tail(url: &str) -> &str {
    let mut end = url.len();
    while end > 0 {
        let keep = match url.as_bytes()[end - 1] {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' => false,
            b')' => balanced(&url[..end], b'(', b')'),
            b']' => balanced(&url[..end], b'[', b']'),
            _ => true,
        };
        if keep {
            break;
        }
        end -= 1;
    }
    &url[..end]
}

/// 括号在这段地址里是否配平——不配平就说明右括号是外面那对的。
fn balanced(url: &str, open: u8, close: u8) -> bool {
    let count = |target: u8| url.bytes().filter(|byte| *byte == target).count();
    count(open) >= count(close)
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
    match_command_inner(ctx, command_name, false)
}

/// Word commands require whitespace or end-of-message after their name.
pub fn match_word_command(ctx: &Context, command_name: &str) -> Option<CommandMatch> {
    match_command_inner(ctx, command_name, true)
}

fn match_command_inner(ctx: &Context, command_name: &str, strict: bool) -> Option<CommandMatch> {
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
                        if strict
                            && rest_of_text
                                .chars()
                                .next()
                                .is_some_and(|c| !c.is_whitespace())
                        {
                            continue;
                        }
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

#[cfg(test)]
mod tests {
    use super::find_url;

    #[test]
    fn urls_stop_where_the_sentence_resumes() {
        // 群里最常见的形态：中文、全角标点直接粘在地址后面。
        assert_eq!(
            find_url(
                "避雷这个中转站https://platform.deepseek.com，pro模型路由到flash，真的是脸都不要了"
            )
            .as_deref(),
            Some("https://platform.deepseek.com")
        );
        assert_eq!(
            find_url("看这个 https://example.com/a/b?x=1&y=2。然后呢").as_deref(),
            Some("https://example.com/a/b?x=1&y=2")
        );
        assert_eq!(
            find_url("链接是「https://example.com/路径」").as_deref(),
            Some("https://example.com/")
        );
        // 半角句尾标点同样不属于地址。
        assert_eq!(
            find_url("see https://example.com, and more").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            find_url("go to https://example.com/docs.").as_deref(),
            Some("https://example.com/docs")
        );
    }

    #[test]
    fn brackets_are_kept_only_when_they_belong_to_the_url() {
        assert_eq!(
            find_url("(https://example.com/a)").as_deref(),
            Some("https://example.com/a")
        );
        // 维基百科那种地址里本来就带括号，配平的就留着。
        assert_eq!(
            find_url("https://en.wikipedia.org/wiki/Rust_(programming_language) 挺好").as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(programming_language)")
        );
    }

    #[test]
    fn only_real_links_come_back() {
        assert_eq!(
            find_url("先 http://127.0.0.1:6520/panel#tab 再说").as_deref(),
            Some("http://127.0.0.1:6520/panel#tab")
        );
        // 百分号编码的中文路径是完整的地址，不能在编码处断开。
        assert_eq!(
            find_url("https://zh.wikipedia.org/wiki/%E4%B8%AD%E6%96%87 这个").as_deref(),
            Some("https://zh.wikipedia.org/wiki/%E4%B8%AD%E6%96%87")
        );
        assert_eq!(find_url("没有链接的一句话"), None);
        assert_eq!(find_url("裸域名 example.com 不算"), None);
        assert_eq!(find_url("https://。"), None);
    }
}
