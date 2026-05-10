use super::memory::{now_secs, rank_memories};
use super::types::{GroupState, PersonaConfig, RecentMsg, UserProfile, mood_label};
use std::collections::{HashMap, HashSet};

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

pub fn format_group_memories(state: &GroupState, half_life: f64, top_k: usize) -> String {
    let top = rank_memories(&state.memories, half_life, top_k);
    if top.is_empty() {
        return "（暂无值得记住的事）".to_string();
    }
    top.iter()
        .enumerate()
        .map(|(i, m)| format!("{}. {}", i + 1, m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_one_user(profile: &UserProfile, half_life: f64, top_notes: usize) -> String {
    let now = now_secs();
    let days_since = ((now - profile.last_seen_at).max(0) as f64 / 86400.0) as u32;
    let recency = if days_since == 0 {
        String::from("今天还在")
    } else if days_since <= 1 {
        String::from("昨天还在")
    } else if days_since < 7 {
        format!("{}天前还在", days_since)
    } else {
        format!("好久不见({}天)", days_since)
    };

    let alias_part = if profile.aliases.is_empty() {
        String::new()
    } else {
        let last_aliases = profile
            .aliases
            .iter()
            .rev()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        format!("（曾用名：{}）", last_aliases)
    };

    let notes = rank_memories(&profile.notes, half_life, top_notes);
    let note_part = if notes.is_empty() {
        "（你还不太了解这个人）".to_string()
    } else {
        notes
            .iter()
            .map(|m| format!("- {}", m.content))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "{name}{alias} [{recency}, 共发言 {cnt} 条]\n{notes}",
        name = profile.last_name,
        alias = alias_part,
        recency = recency,
        cnt = profile.message_count,
        notes = note_part,
    )
}

pub fn format_relevant_users(
    users: &HashMap<String, UserProfile>,
    relevant_ids: &HashSet<String>,
    half_life: f64,
    top_notes: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for id in relevant_ids {
        if let Some(prof) = users.get(id)
            && (!prof.notes.is_empty() || !prof.aliases.is_empty() || prof.message_count > 5)
        {
            lines.push(format_one_user(prof, half_life, top_notes));
        }
    }
    if lines.is_empty() {
        "（群里这些人你还都不熟）".to_string()
    } else {
        lines.join("\n\n")
    }
}

pub fn relevant_user_ids(
    recent: &[RecentMsg],
    trigger: Option<&RecentMsg>,
    self_id: &str,
) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(t) = trigger
        && !t.sender_id.is_empty()
        && t.sender_id != self_id
    {
        set.insert(t.sender_id.clone());
    }
    for m in recent.iter().rev().take(8) {
        if !m.is_self && !m.sender_id.is_empty() && m.sender_id != self_id {
            set.insert(m.sender_id.clone());
        }
    }
    set
}

pub fn build_decide_prompt(
    cfg: &PersonaConfig,
    state: &GroupState,
    users: &HashMap<String, UserProfile>,
    self_id: &str,
    trigger: &RecentMsg,
) -> (String, String) {
    let system = format!(
        "你扮演 QQ 群里的「{nick}」。\n人设：{persona}\n\n你的任务只是判断「这条消息要不要接话」。普通群友不会条条都接，但只要话题有意思、对路、被点名一般会聊几句。如果话题完全无聊、和你毫无关系、刚发过言、或场面尴尬就别接。\n\n严格只输出 JSON：{{\"reply\": true|false, \"urgency\": 0|1|2|3, \"why\": \"<不超过20字>\"}}",
        nick = cfg.nickname,
        persona = cfg.persona_prompt
    );

    let recent = format_recent(&state.recent, &cfg.nickname);
    let trigger_line = format!("{}：{}", trigger.sender_name, trigger.text);
    let mention_hint = if trigger.mentions_self {
        "\n注意：这条@了你或叫了你的名字。"
    } else {
        ""
    };

    let ids = relevant_user_ids(&state.recent, Some(trigger), self_id);
    let user_block = format_relevant_users(users, &ids, cfg.memory_half_life_days, 3);

    let user = format!(
        "你现在的心情：{mood}\n\n你认识的群友：\n{users}\n\n最近的群聊（旧→新）：\n{recent}\n\n刚收到这一条：\n{trig}{hint}\n\n判断你想不想接话，输出 JSON。",
        mood = mood_label(state.mood),
        users = user_block,
        recent = recent,
        trig = trigger_line,
        hint = mention_hint,
    );

    (system, user)
}

pub fn build_reply_prompt(
    cfg: &PersonaConfig,
    state: &GroupState,
    users: &HashMap<String, UserProfile>,
    self_id: &str,
    trigger: &RecentMsg,
) -> (String, String) {
    let allow_react = if cfg.allow_emoji_react {
        "- 如果你只是想轻轻反应一下（看到了/同意/笑了/无语等），可以用 react_emoji_id 表情回应代替发言。常用表情ID：4(得意) 14(微笑) 21(可爱) 38(敲打) 49(拥抱) 76(赞) 78(摇头) 87(便便) 124(OK) 144(喝彩) 178(笑哭) 181(发抖) 182(笑哭脸) 183(我最美) 187(幽灵) 201(点赞) 203(嘿哈) 277(汪汪) 281(吃瓜) 282(哦豁)。用了表情就不要 messages 了。"
    } else {
        ""
    };

    let allow_quote = if cfg.allow_reply_quote {
        "- reply_first=true 表示用 QQ 的「引用回复」功能引用这条消息（适合明显在回应它的时候）"
    } else {
        ""
    };
    let allow_at = if cfg.allow_at_user {
        "- at_first=true 表示首条消息@对方（点名时用，别滥用）"
    } else {
        ""
    };

    let system = format!(
        "你扮演 QQ 群里的「{nick}」，正在跟群友聊天。\n人设：{persona}\n\n说话规则（必须严格遵守）：\n- 像真人在群里说话，每条最多 {max} 字\n- 拆成 1～{seg} 条短消息发出（messages 数组）。真人是一条接一条断断续续打字，不会把所有想法压在一条里。但也别为拆而拆——简单的就一条够了\n- 不用 AI 助手腔（不说\"我可以帮您\"\"作为AI\"\"很高兴为您\"等），不要客套\n- 不分点列、不用 markdown、不写表情符号包裹的解释\n- 偶尔用网络梗、口头禅、错别字、语气词（嗯、啊、哦、唉、卧槽、笑死、绷不住）\n- 心情会影响语气；不开心就别强装热情\n- 对认识的人可以直接用名字、调侃、提以前聊过的事\n- 绝不暴露自己是 AI/机器人/程序；被怀疑就自然否认或转移话题\n{allow_quote}\n{allow_at}\n{allow_react}\n\n严格输出 JSON（顺序无所谓，字段缺省按默认）：\n{{\n  \"messages\": [\"<第1条>\", \"<第2条>\", ...],   // 想了想不发就空数组\n  \"reply_first\": false,\n  \"at_first\": false,\n  \"react_emoji_id\": null,                       // 数字或null；非null时优先用它\n  \"mood_shift\": <-0.3 到 0.3 之间的小数>,\n  \"remember_global\": \"<群级值得长期记住的事；否则空>\",\n  \"remember_user\": \"<关于这个发言人的事实/特征/偏好/约定；否则空>\"\n}}",
        nick = cfg.nickname,
        persona = cfg.persona_prompt,
        max = cfg.max_reply_chars,
        seg = cfg.max_segments,
        allow_quote = allow_quote,
        allow_at = allow_at,
        allow_react = allow_react,
    );

    let memories = format_group_memories(state, cfg.memory_half_life_days, 6);
    let recent = format_recent(&state.recent, &cfg.nickname);
    let trigger_line = format!("{}：{}", trigger.sender_name, trigger.text);
    let mention_hint = if trigger.mentions_self {
        "\n（这条@了你或叫了你的名字）"
    } else {
        ""
    };

    let ids = relevant_user_ids(&state.recent, Some(trigger), self_id);
    let user_block = format_relevant_users(users, &ids, cfg.memory_half_life_days, 4);

    let user = format!(
        "你现在的心情：{mood}\n\n你认识的群友（含本次发言人）：\n{users}\n\n你记得的群里的事：\n{mem}\n\n群里最近聊的（旧→新）：\n{recent}\n\n你想接的这一条：\n{trig}{hint}\n\n输出 JSON。",
        mood = mood_label(state.mood),
        users = user_block,
        mem = memories,
        recent = recent,
        trig = trigger_line,
        hint = mention_hint,
    );

    (system, user)
}

pub fn build_initiate_prompt(
    cfg: &PersonaConfig,
    state: &GroupState,
    users: &HashMap<String, UserProfile>,
    self_id: &str,
) -> (String, String) {
    let system = format!(
        "你扮演 QQ 群里的「{nick}」。群里现在有点安静，你想主动起个话题或者发个无聊的吐槽，像真人一样。\n人设：{persona}\n\n注意：\n- 不要太刻意，不要假装关心、不要发\"大家在干嘛\"这种群机器人感很重的话\n- 真人主动发言更像是：突然想到一件事、看到啥想吐槽、刚遇到点啥、或者接续之前没聊完的话题\n- 1～{seg} 条短消息，每条不超过 {max} 字\n- 想了想还是别开口的话，messages 留空数组\n\n严格输出 JSON：\n{{\n  \"messages\": [\"<第1条>\", \"<第2条>\"],\n  \"reply_first\": false,\n  \"at_first\": false,\n  \"react_emoji_id\": null,\n  \"mood_shift\": <-0.3 到 0.3>,\n  \"remember_global\": \"\",\n  \"remember_user\": \"\"\n}}",
        nick = cfg.nickname,
        persona = cfg.persona_prompt,
        max = cfg.max_reply_chars,
        seg = cfg.max_segments,
    );

    let memories = format_group_memories(state, cfg.memory_half_life_days, 6);
    let recent = format_recent(&state.recent, &cfg.nickname);

    let ids = relevant_user_ids(&state.recent, None, self_id);
    let user_block = format_relevant_users(users, &ids, cfg.memory_half_life_days, 3);

    let now = now_secs();
    let quiet_mins = ((now - state.last_msg_at).max(0) / 60) as i64;
    let quiet_hint = if quiet_mins >= 60 {
        format!("已经安静了 {} 小时多。", quiet_mins / 60)
    } else {
        format!("已经安静了 {} 分钟。", quiet_mins.max(0))
    };

    let user = format!(
        "你现在的心情：{mood}\n\n你认识的群友：\n{users}\n\n你记得群里的事：\n{mem}\n\n上次聊的（旧→新）：\n{recent}\n\n{quiet}\n\n想发就发，输出 JSON。",
        mood = mood_label(state.mood),
        users = user_block,
        mem = memories,
        recent = recent,
        quiet = quiet_hint,
    );

    (system, user)
}
