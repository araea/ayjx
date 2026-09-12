//! 对话模板房间：一间房对应一个当下主流的对话模型，开箱即用。
//!
//! 这些房间本身没有任何新机制——就是「模型选好了、提示词留空」的普通房间。它们的
//! 用处是让人不必先知道中转站上有哪些 id、也不必记住 `##名称 模型` 怎么写，`/#`
//! 里直接挑一间就能聊；觉得顺手再自己写提示词（`房间$...`），或复制一份改。
//!
//! **名字**一律是 `聊·<两三字>`。房间指令是前缀匹配的（见
//! [`super::parser::parse_agent_cmd`]），房间叫「GPT」就意味着任何以「GPT」开头的
//! 闲聊都会被当成对话指令。中间那个 `·` 让误触发几乎不可能发生，又不影响中文
//! 输入法直接打出来——和 `画图预设` 是同一套办法，[`super::presets`]。
//!
//! **模型按关键字取**：站点上同一家的 id 会带各种后缀（`-thinking`、日期快照），
//! 起始排序下关键字命中的第一条就是不带后缀的那个；列表还没拉回来时用兜底 id。
//! 房间一旦建出来就不再跟着列表变——管理员改过的模型不该被下次启动改回去。
//!
//! **删掉的不会复活**：建过的名字记在 `seeded_presets` 里，和画图预设共用这份账。
//! 管理员删掉哪间就是不要哪间；新增预设只会补建没见过的那几间。

use super::types::{Agent, Config, ENGINE_CHAT};

/// 这些房间在 `/#` 列表里单独成区。
pub(crate) const SECTION: &str = "对话预设";

/// 模板房间名的前缀。挡住误触发，也让它们在列表里排在一起。
pub(crate) const PREFIX: &str = "聊·";

/// 一间对话模板房间。
pub(crate) struct Preset {
    /// 房间名，含 [`PREFIX`]。
    pub name: &'static str,
    /// 列表里那一行说明。
    pub desc: &'static str,
    /// 模型关键字，小写、子串匹配；站点上同系列有多条 id 时取排序最靠前的。
    keyword: &'static str,
    /// 模型列表还没拉回来时用的兜底 id。
    fallback: &'static str,
}

/// 模板清单：一间房一个当下公认好用的对话模型，按厂商排。
///
/// 提示词一律留空。内置 agent 早就不再默认塞「你是一个有帮助的助手」，模板房同理：
/// 「拿哪个模型聊」和「怎么聊」是两件事，前者开箱即用，后者该由房间的主人自己写。
pub(crate) const PRESETS: &[Preset] = &[
    Preset {
        name: "聊·GPT",
        desc: "OpenAI GPT-5.6",
        keyword: "gpt-5.6",
        fallback: "gpt-5.6-luna",
    },
    Preset {
        name: "聊·Opus",
        desc: "Claude Opus 5",
        keyword: "claude-opus-5",
        fallback: "claude-opus-5",
    },
    Preset {
        name: "聊·双子",
        desc: "Gemini 3.1 Pro",
        keyword: "gemini-3.1-pro-preview",
        fallback: "gemini-3.1-pro-preview",
    },
    Preset {
        name: "聊·Grok",
        desc: "xAI Grok 4.6",
        keyword: "grok-4.6",
        fallback: "grok-4.6",
    },
    Preset {
        name: "聊·深度",
        desc: "DeepSeek V4 Pro",
        keyword: "deepseek-v4-pro",
        fallback: "deepseek-v4-pro",
    },
    Preset {
        name: "聊·Kimi",
        desc: "Moonshot Kimi K3",
        keyword: "kimi-k3",
        fallback: "kimi-k3",
    },
    Preset {
        name: "聊·通义",
        desc: "阿里通义千问 3.8 Max",
        keyword: "qwen3.8-max",
        fallback: "qwen3.8-max",
    },
    Preset {
        name: "聊·智谱",
        desc: "智谱 GLM-5.3",
        keyword: "glm-5.3",
        fallback: "glm-5.3",
    },
    Preset {
        name: "聊·海螺",
        desc: "MiniMax M2.7",
        keyword: "minimax-m2.7",
        fallback: "MiniMax-M2.7",
    },
    Preset {
        name: "聊·小米",
        desc: "小米 Mimo v2.5",
        keyword: "mimo-v2.5",
        fallback: "mimo-v2.5",
    },
];

/// 按关键字从站点实际在售的列表里挑模型；挑不到就用兜底 id。
fn resolve_model(models: &[String], preset: &Preset) -> String {
    models
        .iter()
        .find(|model| model.to_lowercase().contains(preset.keyword))
        .cloned()
        .unwrap_or_else(|| preset.fallback.to_string())
}

/// 建出还没建过的模板房间，返回新建了几间。
///
/// 判重看两处：`seeded_presets`（建过一次就不再建，删掉的不会复活）和现有房间名
/// （管理员自己占了这个名字时不覆盖）。已存在的同名房间一个字都不动。
pub(crate) fn seed(config: &mut Config) -> usize {
    let mut created = 0;
    for preset in PRESETS {
        if config
            .seeded_presets
            .iter()
            .any(|name| name.eq_ignore_ascii_case(preset.name))
        {
            continue;
        }
        config.seeded_presets.push(preset.name.to_string());
        if config
            .agents
            .iter()
            .any(|agent| agent.name.eq_ignore_ascii_case(preset.name))
        {
            continue;
        }
        let model = resolve_model(&config.models, preset);
        let mut room = Agent::new(preset.name, &model, "", preset.desc);
        room.set_engine(ENGINE_CHAT, &model);
        room.section = SECTION.to_string();
        config.agents.push(room);
        created += 1;
    }
    created
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::oai::parser::valid_agent_name;

    /// 名字得能被房间指令认出来，又不能被日常聊天误触发。
    #[test]
    fn template_names_are_addressable_but_hard_to_trigger_by_accident() {
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(
                valid_agent_name(preset.name),
                "{} 不是合法房间名",
                preset.name
            );
            assert!(preset.name.starts_with(PREFIX), "{}", preset.name);
            assert!(seen.insert(preset.name.to_lowercase()), "{} 重名", preset.name);
            assert!(!preset.desc.is_empty());
            // 描述在列表里只显示 20 个字，超了就看不出这间房是干什么的。
            assert!(preset.desc.chars().count() <= 20, "{}", preset.desc);
        }
        // 前缀后面一定还有字：光一个「聊·」既不像房间名，也拦不住误触发。
        assert!(PRESETS.iter().all(|preset| preset.name.chars().count() > 2));
    }

    /// 站点上同系列有多条 id 时挑不带后缀的那条，挑不到就用兜底 id。
    #[test]
    fn the_model_is_the_plain_one_from_the_site_or_the_fallback() {
        let models: Vec<String> = [
            "claude-opus-5",
            "claude-opus-5-thinking",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "glm-5.3",
            "glm-5.3-flash",
            "mimo-v2.5",
            "mimo-v2.5-pro",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let pick = |name: &str| {
            let preset = PRESETS.iter().find(|p| p.name == name).unwrap();
            resolve_model(&models, preset)
        };
        assert_eq!(pick("聊·Opus"), "claude-opus-5");
        assert_eq!(pick("聊·GPT"), "gpt-5.6-luna");
        assert_eq!(pick("聊·智谱"), "glm-5.3");
        assert_eq!(pick("聊·小米"), "mimo-v2.5");
        // 列表里没有这家的模型：用兜底 id，而不是硬塞一条不相干的。
        assert_eq!(pick("聊·Kimi"), "kimi-k3");
    }

    /// 建过一次就不再建；管理员删掉的房间不会在下次启动时复活。
    #[test]
    fn seeding_is_idempotent_and_deletions_stick() {
        let mut config = Config::default();
        assert_eq!(seed(&mut config), PRESETS.len());
        assert_eq!(config.agents.len(), PRESETS.len());
        let room = config.agents.iter().find(|a| a.name == "聊·GPT").unwrap();
        assert_eq!(room.model, "gpt-5.6-luna");
        assert_eq!(room.section, SECTION);
        assert!(room.system_prompt.is_empty());
        assert!(!room.uses_pi());

        // 再跑一次什么都不建。
        assert_eq!(seed(&mut config), 0);
        // 删掉一间，下次启动也不再建。
        config.agents.retain(|agent| agent.name != "聊·GPT");
        assert_eq!(seed(&mut config), 0);
        assert!(!config.agents.iter().any(|a| a.name == "聊·GPT"));

        // 新增预设只补建没见过的那一间。
        config.seeded_presets.retain(|name| name != "聊·Kimi");
        config.agents.retain(|agent| agent.name != "聊·Kimi");
        assert_eq!(seed(&mut config), 1);
    }

    /// 同名房间已被占用时不覆盖，但也不再反复尝试。
    #[test]
    fn an_existing_room_of_the_same_name_is_left_alone() {
        let mut config = Config::default();
        let mut mine = Agent::new("聊·GPT", "gpt-5.5", "我自己的提示词", "我的房间");
        mine.set_engine(ENGINE_CHAT, "gpt-5.5");
        config.agents.push(mine);
        seed(&mut config);
        let room = config.agents.iter().find(|a| a.name == "聊·GPT").unwrap();
        assert_eq!(room.system_prompt, "我自己的提示词");
        assert_eq!(room.model, "gpt-5.5");
        assert!(room.section.is_empty());
        assert_eq!(config.agents.iter().filter(|a| a.name == "聊·GPT").count(), 1);
    }
}
