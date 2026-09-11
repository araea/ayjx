//! 群聊工具的类型化动作。只把普通群成员的操作暴露给人格。
use super::window::Turn;
use crate::message::Message;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Part {
    Text {
        text: String,
    },
    At {
        user_id: String,
    },
    Face {
        id: String,
    },
    Image {
        source: String,
    },
    File {
        source: String,
        name: String,
    },
    Audio {
        source: String,
    },
    Video {
        source: String,
    },
    /// 复用当前群某条消息中的图片/商城表情，保留原始参数。
    Sticker {
        message_id: String,
        #[serde(default)]
        index: usize,
    },
    Dice,
    Rps,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Send {
        parts: Vec<Part>,
        #[serde(default)]
        reply_to: Option<String>,
    },
    Poke {
        user_id: String,
    },
    Like {
        user_id: String,
        #[serde(default = "one")]
        times: u8,
    },
    React {
        message_id: String,
        emoji_id: String,
        #[serde(default)]
        remove: bool,
    },
    Recall {
        message_id: String,
    },
    /// 转发已有消息保持真实作者；整理新内容时统一署机器人自己。
    Forward {
        #[serde(default)]
        message_ids: Vec<String>,
        #[serde(default)]
        texts: Vec<String>,
    },
}
fn one() -> u8 {
    1
}

pub(crate) fn id(raw: &str) -> Result<i64> {
    let value: i64 = raw.parse()?;
    ensure!(value != 0, "ID 用一个非零的数字");
    Ok(value)
}
pub(crate) fn message<'a>(turns: &'a [Turn], raw: &str) -> Result<&'a Turn> {
    let id = id(raw)?;
    turns
        .iter()
        .find(|t| t.message_id == id)
        .ok_or_else(|| anyhow::anyhow!("消息不在本群当前窗口内，先读 satori_context"))
}
pub(crate) fn user(turns: &[Turn], raw: &str) -> Result<i64> {
    let id = id(raw)?;
    ensure!(
        id > 0 && turns.iter().any(|t| t.user_id == id),
        "目标取自当前群记录里的成员"
    );
    Ok(id)
}

impl Action {
    pub(crate) fn validate(&self, turns: &[Turn]) -> Result<()> {
        match self {
            Self::Send { parts, reply_to } => {
                ensure!(
                    !parts.is_empty() && parts.len() <= 16,
                    "消息元素数量须为 1–16"
                );
                if let Some(id) = reply_to {
                    message(turns, id)?;
                }
                let mut chars = 0;
                for part in parts {
                    match part {
                        Part::At { user_id } => {
                            user(turns, user_id)?;
                        }
                        Part::Text { text } => {
                            chars += text.chars().count();
                        }
                        Part::Face { id } => {
                            ensure!(id.parse::<u32>().is_ok(), "表情 ID 用数字");
                        }
                        Part::Sticker { message_id, index } => {
                            sticker(message(turns, message_id)?, *index)?;
                        }
                        Part::File { name, .. } => {
                            ensure!(
                                !name.is_empty()
                                    && name.len() <= 180
                                    && !name.contains(['/', '\\', '\n', '\r']),
                                "文件名无效"
                            );
                        }
                        _ => {}
                    }
                }
                ensure!(
                    chars <= 4000,
                    "一条消息正文最多 4000 字；长材料请整理为文件或合并转发"
                );
                // 复读不占额度也不该发出去：这里拦下来，模型还有机会换一句。
                let body: String = parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                ensure!(
                    !super::tone::echoes(&body, turns),
                    "这句话刚说过，换个说法，或者先放着也行"
                );
            }
            Self::Poke { user_id } => {
                user(turns, user_id)?;
            }
            Self::Like { user_id, times } => {
                user(turns, user_id)?;
                ensure!((1..=10).contains(times), "单次资料卡点赞 1–10 次");
            }
            Self::React {
                message_id,
                emoji_id,
                ..
            } => {
                message(turns, message_id)?;
                ensure!(emoji_id.parse::<u32>().is_ok(), "表态 ID 用数字");
            }
            Self::Recall { message_id } => {
                ensure!(message(turns, message_id)?.from_me, "撤回作用于自己发出的消息");
            }
            Self::Forward { message_ids, texts } => {
                ensure!(
                    (1..=12).contains(&(message_ids.len() + texts.len())),
                    "合并转发须有 1–12 个节点"
                );
                ensure!(
                    texts.iter().map(|s| s.chars().count()).sum::<usize>() <= 16000,
                    "转发正文过长"
                );
                for id in message_ids {
                    let turn = message(turns, id)?;
                    ensure!(!turn.elements.0.is_empty(), "该条记录没有可转发的原始内容");
                    ensure!(
                        !turn
                            .elements
                            .0
                            .iter()
                            .any(|s| matches!(s.type_.as_str(), "node" | "forward" | "poke")),
                        "不支持嵌套转发或转发动作"
                    );
                }
            }
        }
        Ok(())
    }
    pub(crate) fn is_message(&self) -> bool {
        matches!(self, Self::Send { .. } | Self::Forward { .. })
    }
}

pub(crate) fn sticker(turn: &Turn, index: usize) -> Result<Message> {
    let segment = turn
        .elements
        .0
        .iter()
        .filter(|s| matches!(s.type_.as_str(), "image" | "mface"))
        .nth(index)
        .ok_or_else(|| anyhow::anyhow!("这条消息没有对应的图片/表情包"))?;
    Ok(Message(vec![segment.clone()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn turns() -> Vec<Turn> {
        vec![Turn {
            user_id: 42,
            name: "群友".into(),
            text: "hi".into(),
            images: vec![],
            elements: Message::new().image("https://example.com/a.gif"),
            message_id: 123,
            from_me: false,
            mentions_me: false,
            at: 0,
        }]
    }
    #[test]
    fn actions_require_known_targets_and_own_recall() {
        let turns = turns();
        for raw in [
            r#"{"action":"poke","user_id":"999"}"#,
            r#"{"action":"recall","message_id":"123"}"#,
            r#"{"action":"like","user_id":"42","times":11}"#,
            r#"{"action":"send","parts":[{"type":"at","user_id":"all"}]}"#,
        ] {
            assert!(
                serde_json::from_str::<Action>(raw)
                    .unwrap()
                    .validate(&turns)
                    .is_err(),
                "{raw}"
            );
        }
        assert!(serde_json::from_str::<Action>(r#"{"action":"send","reply_to":"123","parts":[{"type":"sticker","message_id":"123"}]}"#).unwrap().validate(&turns).is_ok());
        assert!(
            serde_json::from_str::<Action>(
                r#"{"action":"poke","user_id":"42","guild_id":"other"}"#
            )
            .is_err()
        );
    }
    #[test]
    fn saying_the_same_thing_twice_is_rejected_before_it_costs_a_write() {
        let mut turns = turns();
        turns.push(Turn {
            user_id: 10_000,
            name: "我".into(),
            text: "那你重启一下路由器试试 不行再说".into(),
            images: vec![],
            elements: Message::new(),
            message_id: 124,
            from_me: true,
            mentions_me: false,
            at: 0,
        });
        let echo: Action = serde_json::from_str(
            r#"{"action":"send","parts":[{"type":"text","text":"那你重启一下路由器试试，不行再说"}]}"#,
        )
        .unwrap();
        assert!(echo.validate(&turns).is_err());
        let fresh: Action = serde_json::from_str(
            r#"{"action":"send","parts":[{"type":"text","text":"那是驱动的问题 跟路由器无关"}]}"#,
        )
        .unwrap();
        fresh.validate(&turns).unwrap();
    }

    #[test]
    fn ids_keep_qq_precision_and_stickers_keep_resources() {
        assert_eq!(id("7837409278651234567").unwrap(), 7837409278651234567);
        let mut ts = turns();
        ts[0].from_me = true;
        Action::Recall {
            message_id: "123".into(),
        }
        .validate(&ts)
        .unwrap();
        assert_eq!(sticker(&ts[0], 0).unwrap().0[0].type_, "image");
        assert!(sticker(&ts[0], 1).is_err());
    }
}
