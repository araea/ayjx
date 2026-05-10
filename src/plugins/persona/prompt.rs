use super::memory::rank_memories;
use super::types::{GroupState, PersonaConfig, RecentMsg, mood_label};

pub fn format_recent(recent: &[RecentMsg], nickname: &str) -> String {
    if recent.is_empty() {
        return "（群里刚开始聊）".to_string();
    }
    recent
        .iter()
        .map(|m| {
            let who = if m.is_self {
                format!("{}(我)", nickname)
            } else {
                m.sender_name.clone()
            };
            format!("{}：{}", who, m.text.replace('\n', " "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_memories(state: &GroupState, half_life: f64, top_k: usize) -> String {
    let top = rank_memories(state, half_life, top_k);
    if top.is_empty() {
        return "（暂无值得记住的事）".to_string();
    }
    top.iter()
        .enumerate()
        .map(|(i, m)| format!("{}. {}", i + 1, m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_decide_prompt(
    cfg: &PersonaConfig,
    state: &GroupState,
    trigger: &RecentMsg,
) -> (String, String) {
    let system = format!(
        "你正在扮演「{nick}」，潜伏在一个 QQ 群。\n人设：{persona}\n\n你的任务只是判断「这条消息要不要接话」。要像真人——绝大多数时候选择不说话，只有真的被勾起兴趣、被点名、或情绪上头才会回。如果话题无聊、和你无关、你刚说过话、或场面尴尬，就不要回。\n\n严格只输出一个 JSON：{{\"reply\": true/false, \"urgency\": 0|1|2|3, \"why\": \"<不超过20字理由>\"}}",
        nick = cfg.nickname,
        persona = cfg.persona_prompt
    );

    let recent = format_recent(&state.recent, &cfg.nickname);
    let trigger_line = format!("{}：{}", trigger.sender_name, trigger.text);
    let mention_hint = if trigger.mentions_self {
        "\n注意：这条消息@了你或叫了你的名字。"
    } else {
        ""
    };

    let user = format!(
        "当前心情：{mood}\n\n最近的群聊（旧→新）：\n{recent}\n\n刚收到的消息：\n{trigger}{hint}\n\n判断你想不想接话，输出 JSON。",
        mood = mood_label(state.mood),
        recent = recent,
        trigger = trigger_line,
        hint = mention_hint,
    );

    (system, user)
}

pub fn build_reply_prompt(
    cfg: &PersonaConfig,
    state: &GroupState,
    trigger: &RecentMsg,
) -> (String, String) {
    let system = format!(
        "你正在扮演「{nick}」，QQ 群里一个普通人。\n人设：{persona}\n\n说话规则（必须严格遵守）：\n- 像真人在群里发言，最多 {max} 字\n- 通常一句话，可以是几个字、单字、或带个 emoji\n- 不用 AI 助手腔（不说\"我可以帮您\"\"作为AI\"\"很高兴\"等套话），不要客套\n- 不要分点列出内容，不要用 markdown\n- 偶尔可以用网络梗、口头禅、错别字、语气词\n- 心情会影响语气；不开心就别强装热情\n- 绝不暴露自己是 AI/机器人/程序；被怀疑时正常否认或转移话题\n\n输出严格 JSON：\n{{\n  \"reply\": \"<你要发的话；如果想了想还是不发，给空字符串>\",\n  \"mood_shift\": <-0.3 到 0.3 之间的小数，反映这轮聊天对心情的影响>,\n  \"remember\": \"<如果出现一个值得长期记住的事实/约定/称呼/八卦，简短记录；否则空字符串>\"\n}}",
        nick = cfg.nickname,
        persona = cfg.persona_prompt,
        max = cfg.max_reply_chars
    );

    let memories = format_memories(state, cfg.memory_half_life_days, 6);
    let recent = format_recent(&state.recent, &cfg.nickname);
    let trigger_line = format!("{}：{}", trigger.sender_name, trigger.text);
    let mention_hint = if trigger.mentions_self {
        "\n（这条@了你或叫了你的名字）"
    } else {
        ""
    };

    let user = format!(
        "你现在的心情：{mood}\n\n你记得的事：\n{mem}\n\n群里最近聊的（旧→新）：\n{recent}\n\n你想接的这一条：\n{trigger}{hint}\n\n输出 JSON。",
        mood = mood_label(state.mood),
        mem = memories,
        recent = recent,
        trigger = trigger_line,
        hint = mention_hint,
    );

    (system, user)
}
