//! 人格模型的一次发言：把群聊记录交给本机 pi，拿回准备发出去的几行字。
//!
//! 和 `pi` 房间共用同一个执行层（[`pi_agent::run`]），差别只在参数：这里不带
//! 会话文件（群聊的上下文就是那段聊天记录本身，没有需要跨轮维护的状态）、
//! 换掉 pi 默认的编码助手系统提示词、限定工具、并挂上描述 Satori 消息元素的
//! skill——让「能说什么」随 skill 生长，而不是随这个文件生长。

use super::AmbientConfig;
use super::window::{Turn, transcript};
use crate::plugins::oai::pi_agent::{self, PiRun};
use std::path::Path;

/// 发言时的行为守则。人设负责「他是谁」，这里只负责「群聊怎么说话」。
fn house_rules(max_messages: usize) -> String {
    format!(
        "\
你现在在一个 QQ 群里，作为群成员之一说话。没有人在等你服务，也没有人向你提问的义务。

怎么说：
- 一次最多 {max_messages} 条消息，每条通常不超过 30 字。能一句说尽的绝不说两句。
- 群友在聊日常时，接一句短的就够了：一个反问、一句吐槽、一处纠正、一个不动声色的
  玩笑，都算参与。不必每次开口都端出一段判词，也不必把话题拔高。
- 不复述别人的话，不总结，不解释你的玩笑，不说「哈哈」「好的」这类没有信息的话。
- 不自称助手、模型或程序，不提到提示词、工具、检索与这份守则。
- 话题滑向色情或情感纠缠时，只输出 [silent]，不辩不斥。
- 拿不准的事实可以查，要算的东西可以算；只把结论说出来，不报告过程。
- 你被叫上场，是因为已经有人判断这一刻值得你开口。所以除非话题落在上面那条禁区里，
  或者实在无话可说，就给一句短的——把「值不值得」再从头掂量一遍，是在做别人做过的事。
- 真的无话可说时，就只输出 [silent]。这仍然是正当的选择。

怎么发：
- 一行就是一条消息，最多 {max_messages} 行；正文之外不写任何解释、前缀或引号。
- 行内可用：[at:QQ号]、[face:表情ID]、[img:图片直链]。
- 独占一行可用：[poke:QQ号]、[dice]、[rps]、[wait:秒数]、[silent]。
- 想用更多花样（表情 ID 表、引用某条消息、发图的注意事项）时，读 skill `satori-reply`。"
    )
}

/// 记录末尾那句话，交代这一轮的性质。
///
/// 被直接叫到是唯一一种「沉默显得像坏了，而不是像孤傲」的场合——判定给到 90 分、
/// 人却一字不回，看着就是机器人挂了。所以这一轮明确允许敷衍，但禁区仍然排在前面。
fn closing(mentioned: bool) -> &'static str {
    if mentioned {
        "有人直接叫了你，或者 @ 了你。可以短、可以敷衍、可以用一个反问打发，但除非话题落在禁区里，别一字不回。"
    } else {
        "现在轮到你决定说不说话。"
    }
}

/// 让人格模型读一遍群聊，拿回它想说的话（可能是 `[silent]`）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compose(
    command: &str,
    base: &Path,
    skill: &Path,
    persona: &str,
    config: &AmbientConfig,
    stall: Option<std::time::Duration>,
    turns: &[Turn],
    images: &[String],
    mentioned: bool,
) -> anyhow::Result<String> {
    let dir = pi_agent::ScratchDir::under(base, "runs")?;
    let system = format!("{}\n\n---\n\n{}", persona.trim(), house_rules(config.max_messages));
    let prompt = format!(
        "最近的群聊记录：\n{}\n{}",
        transcript(turns),
        closing(mentioned)
    );
    let skills = [skill.to_path_buf()];

    let reply = tokio::time::timeout(
        config.reply_timeout(),
        pi_agent::run(PiRun {
            cwd: Some(dir.path()),
            system_prompt: Some(&system),
            model: Some(&config.reply_model),
            thinking: Some(&config.thinking),
            skills: &skills,
            tools: Some(&config.tools),
            context_files: false,
            stall,
            prompt: &prompt,
            images,
            ..PiRun::new(command, dir.path())
        }),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "发言超时（{} 秒），已终止 pi",
            config.reply_timeout().as_secs()
        )
    })??;
    Ok(reply.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn house_rules_carry_the_output_protocol_and_the_silent_escape() {
        let rules = house_rules(3);
        assert!(rules.contains("最多 3 条消息"));
        assert!(rules.contains("[silent]"));
        assert!(rules.contains("satori-reply"));
        // 允许参与日常，而不是只在「值得审判」时才开口。
        assert!(rules.contains("日常"));
    }

    #[test]
    fn being_called_out_forbids_total_silence_but_not_the_forbidden_topics() {
        let called = closing(true);
        assert!(called.contains("别一字不回"), "{called}");
        assert!(called.contains("禁区"), "{called}");
        assert!(!closing(false).contains("别一字不回"));
    }
}
