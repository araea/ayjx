//! 人格自行选择短期关注对象/话题；只影响后续新消息的判断，不会定时自言自语。
use super::window::Turn;
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct Focus {
    pub users: Vec<i64>,
    pub topic: String,
    pub until: Instant,
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    users: Vec<i64>,
    #[serde(default)]
    topic: String,
    seconds: u64,
}

/// None 保持现状，Some(None) 主动离场，Some(Some(..)) 更新关注。
/// 内部控制行无论格式是否正确都不发到群里。
pub(crate) fn extract(
    raw: &str,
    turns: &[Turn],
    max_seconds: u64,
) -> (String, Option<Option<Focus>>) {
    let mut lines = Vec::new();
    let mut update = None;
    for line in raw.lines() {
        let clean = line
            .trim()
            .trim_start_matches("- ")
            .trim_start_matches("* ");
        if let Some(body) = clean.strip_prefix("[focus:") {
            if let Some(body) = body.strip_suffix(']')
                && let Ok(request) = serde_json::from_str::<Request>(body.trim())
            {
                if request.seconds == 0 || max_seconds == 0 {
                    update = Some(None);
                } else {
                    let users = request
                        .users
                        .into_iter()
                        .filter(|id| {
                            *id > 0
                                && turns
                                    .iter()
                                    .any(|turn| !turn.from_me && turn.user_id == *id)
                        })
                        .take(3)
                        .collect::<Vec<_>>();
                    let topic = request
                        .topic
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(80)
                        .collect::<String>();
                    if !users.is_empty() || !topic.is_empty() {
                        update = Some(Some(Focus {
                            users,
                            topic,
                            until: Instant::now()
                                + Duration::from_secs(request.seconds.min(max_seconds).min(600)),
                        }));
                    }
                }
            }
            continue;
        }
        lines.push(line);
    }
    (lines.join("\n"), update)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_persona_can_follow_a_topic_without_sending_control_lines() {
        let (body, update) = extract(
            "[focus:{\"users\":[999],\"topic\":\"这游戏\",\"seconds\":99999}]\n[silent]",
            &[],
            300,
        );
        assert_eq!(body, "[silent]");
        let focus = update.unwrap().unwrap();
        assert!(focus.users.is_empty());
        assert_eq!(focus.topic, "这游戏");
        assert!(focus.until.saturating_duration_since(Instant::now()) <= Duration::from_secs(300));
    }

    #[test]
    fn malformed_control_lines_do_not_leak_and_leaving_is_explicit() {
        let (body, update) = extract("- [focus:broken]\n接着说", &[], 300);
        assert_eq!(body, "接着说");
        assert!(update.is_none());
        assert!(matches!(
            extract("[focus:{\"seconds\":0}]", &[], 300).1,
            Some(None)
        ));
        assert!(matches!(
            extract("[focus:{\"topic\":\"x\",\"seconds\":30}]", &[], 0).1,
            Some(None)
        ));
        assert!(extract("随便看看", &[], 300).1.is_none());
    }
}
