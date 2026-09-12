use super::utils::normalize;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Action {
    Chat,
    Regenerate,
    Stop,
    #[default]
    Create,
    Copy,
    Rename,
    SetDesc,
    Delete,
    List,
    SetModel,
    SetSearch,
    SetPrompt,
    ViewPrompt,
    ListModels,
    ViewAll(Scope),
    ViewAt(Scope),
    Export(Scope),
    EditAt(Scope),
    DeleteAt(Scope),
    ClearHistory(Scope),
    ClearAllPublic,
    ClearEverything,
    Help,
    AutoFillDescriptions(String),
    UpdateApi(String, String),
}

#[derive(Debug, Clone)]
pub struct Command {
    pub agent: String,
    pub action: Action,
    pub args: String,
    pub indices: Vec<usize>,
    pub private_reply: bool,
    pub text_mode: bool,
    pub temp_mode: bool,
}

impl Command {
    pub fn new(agent: &str, action: Action) -> Self {
        Self {
            agent: agent.to_string(),
            action,
            args: String::new(),
            indices: Vec::new(),
            private_reply: false,
            text_mode: false,
            temp_mode: false,
        }
    }
}

pub fn parse_global(raw: &str, prefixes: &[String]) -> Option<Command> {
    let norm = normalize(raw.trim());

    // `oai` 是词指令而非符号指令，帮助里显示为带前缀的写法（默认 `/oai`）。
    // 这里同时接受带前缀与裸写法，避免「帮助说 /oai、实现只认 oai」。
    let worded = prefixes
        .iter()
        .filter(|prefix| !prefix.is_empty())
        .find_map(|prefix| norm.strip_prefix(prefix.as_str()))
        .unwrap_or(norm.as_str());

    if worded.starts_with("oai") {
        let rest = worded.get(3..).unwrap_or("").trim();
        if rest.is_empty() {
            return Some(Command::new("", Action::Help));
        }
        if let Some((u, k)) = super::utils::parse_api(rest) {
            return Some(Command::new("", Action::UpdateApi(u, k)));
        }
    }
    if norm == "/#" {
        return Some(Command::new("", Action::List));
    }
    if norm == "/%" {
        return Some(Command::new("", Action::ListModels));
    }
    if norm == "-*" {
        return Some(Command::new("", Action::ClearAllPublic));
    }
    if norm == "-*!" {
        return Some(Command::new("", Action::ClearEverything));
    }
    if norm.starts_with("##:") {
        let args = norm.get(3..).unwrap_or("").trim().to_string();
        return Some(Command::new("", Action::AutoFillDescriptions(args)));
    }
    None
}

/// `-` 在房间指令里是删除第 N 条的写法（`助手-1`），所以名字里通常不能出现；
/// 历史上的 `pi-*` 房间是唯一例外，改名会丢掉它们的聊天记录，继续放行。
pub(crate) fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 7
        && !name
            .chars()
            .any(|c| c.is_whitespace() || "&\"#~/ _'!@$%:*".contains(c))
        && (!name.contains('-') || super::agent::legacy_pi_name(name))
}

pub fn parse_create(raw: &str) -> Option<(String, String, String, String)> {
    let norm = normalize(raw.trim());
    if !norm.starts_with("##") {
        return None;
    }

    let start_pos = norm.find("##").unwrap() + "##".len();
    let after = &raw.trim()[start_pos..];
    let name_end = after
        .find(|c: char| c.is_whitespace() || c == '(' || c == '（')
        .unwrap_or(after.len());
    let name = after[..name_end].trim().to_string();

    if !valid_agent_name(&name) {
        return None;
    }

    let rest = &after[name_end..];
    let (desc, after_desc) = if rest.starts_with('(') || rest.starts_with('（') {
        if let Some(pos) = rest.find(')').or_else(|| rest.find('）')) {
            (rest[1..pos].to_string(), &rest[pos + 1..])
        } else {
            (String::new(), rest)
        }
    } else {
        (String::new(), rest)
    };

    let parts: Vec<&str> = after_desc.split_whitespace().collect();
    let model = parts.first().unwrap_or(&"").to_string();
    if model.chars().count() > 50 {
        return None;
    }
    let prompt = if parts.len() > 1 {
        parts[1..].join(" ")
    } else {
        String::new()
    };

    Some((name, desc, model, prompt))
}

pub fn parse_delete_agent(raw: &str, agents: &[String]) -> Option<String> {
    let norm = normalize(raw.trim());
    if !norm.starts_with("-#") {
        return None;
    }
    let name = norm[2..].trim();
    agents
        .iter()
        .find(|a| a.eq_ignore_ascii_case(name))
        .cloned()
}

pub fn parse_agent_cmd(raw: &str, agents: &[String]) -> Option<Command> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let norm = normalize(raw);
    let chars: Vec<char> = norm.chars().collect();

    let mut char_idx = 0;
    let mut private_reply = false;
    let mut text_mode = false;
    let mut temp_mode = false;

    while char_idx < chars.len() {
        match chars[char_idx] {
            '&' => {
                private_reply = true;
                char_idx += 1;
            }
            '"' => {
                text_mode = true;
                char_idx += 1;
            }
            '~' => {
                temp_mode = true;
                char_idx += 1;
            }
            _ => break,
        }
    }

    let byte_idx: usize = chars.iter().take(char_idx).map(|c| c.len_utf8()).sum();
    let content = &norm[byte_idx..];

    let mut agent_name = String::new();
    let mut match_char_len = 0;
    let mut sorted = agents.to_vec();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.chars().count()));

    for name in &sorted {
        let name_lower = name.to_lowercase();
        let content_lower = content.to_lowercase();
        if content_lower.starts_with(&name_lower) {
            agent_name = name.clone();
            match_char_len = name.chars().count();
            break;
        }
    }

    if agent_name.is_empty() {
        return None;
    }

    let match_byte_len: usize = content
        .chars()
        .take(match_char_len)
        .map(|c| c.len_utf8())
        .sum();
    let suffix = content[match_byte_len..].trim();

    let raw_suffix = {
        let prefix_bytes: usize = raw.chars().take(char_idx).map(|c| c.len_utf8()).sum();
        let agent_bytes: usize = raw[prefix_bytes..]
            .chars()
            .take(match_char_len)
            .map(|c| c.len_utf8())
            .sum();
        raw[prefix_bytes + agent_bytes..].trim()
    };

    let (action, args, indices) = parse_suffix(suffix, raw_suffix, private_reply);

    Some(Command {
        agent: agent_name,
        action,
        args,
        indices,
        private_reply,
        text_mode,
        temp_mode,
    })
}

fn parse_suffix(norm: &str, raw: &str, has_priv_prefix: bool) -> (Action, String, Vec<usize>) {
    let s = norm.trim();
    let r = raw.trim();

    if s.is_empty() {
        return (Action::Chat, r.to_string(), vec![]);
    }
    if s == "!" {
        return (Action::Stop, String::new(), vec![]);
    }

    if s.starts_with("~#") {
        let skip_len = if r.starts_with("～＃") {
            "～＃".len()
        } else if r.starts_with("～#") {
            "～#".len()
        } else if r.starts_with("~＃") {
            "~＃".len()
        } else {
            "~#".len()
        };
        let arg = r.get(skip_len..).unwrap_or("").trim();
        return (Action::Copy, arg.to_string(), vec![]);
    }

    if s.starts_with("~=") {
        let skip_len = if r.starts_with("～＝") {
            "～＝".len()
        } else if r.starts_with("～=") {
            "～=".len()
        } else if r.starts_with("~＝") {
            "~＝".len()
        } else {
            "~=".len()
        };
        let arg = r.get(skip_len..).unwrap_or("").trim();
        return (Action::Rename, arg.to_string(), vec![]);
    }

    if s.starts_with('~') {
        let skip_len = if r.starts_with('～') {
            '～'.len_utf8()
        } else {
            '~'.len_utf8()
        };
        let arg = r.get(skip_len..).unwrap_or("").trim();
        return (Action::Regenerate, arg.to_string(), vec![]);
    }

    if s.starts_with(':') && !s.starts_with(":/") {
        let skip_len = if r.starts_with('：') {
            '：'.len_utf8()
        } else {
            ':'.len_utf8()
        };
        let arg = r.get(skip_len..).unwrap_or("").trim();
        return (Action::SetDesc, arg.to_string(), vec![]);
    }

    if s.starts_with('%') {
        let arg = r.get(1..).unwrap_or("").trim();
        return (Action::SetModel, arg.to_string(), vec![]);
    }

    // `?` 是「联网搜索」。符号表里再没有比问号更贴切的了：它就是一个问句的收尾。
    // 后面跟的词交给 [`search_choice`] 解释，认不出来时回到 `args` 里让调用方提示用法。
    if s.starts_with('?') {
        let skip_len = if r.starts_with('？') {
            '？'.len_utf8()
        } else {
            '?'.len_utf8()
        };
        let arg = r.get(skip_len..).unwrap_or("").trim();
        return (Action::SetSearch, arg.to_string(), vec![]);
    }

    if s == "/$" {
        return (Action::ViewPrompt, String::new(), vec![]);
    }
    if s.starts_with('$') {
        let arg = r.get(1..).unwrap_or("").trim();
        return (Action::SetPrompt, arg.to_string(), vec![]);
    }

    let (has_local_priv, clean, clean_raw) = if let Some(stripped) = s.strip_prefix('&') {
        (true, stripped, r.strip_prefix('&').unwrap_or("").trim())
    } else {
        (false, s, r)
    };

    let scope = if has_priv_prefix || has_local_priv {
        Scope::Private
    } else {
        Scope::Public
    };

    if clean == "/*" {
        return (Action::ViewAll(scope), String::new(), vec![]);
    }

    if clean.starts_with('/') && clean.len() > 1 {
        let idx_part = &clean[1..];
        let indices = super::utils::parse_indices(idx_part);
        if !indices.is_empty() {
            return (Action::ViewAt(scope), String::new(), indices);
        }
    }

    if clean == "_*" {
        return (Action::Export(scope), String::new(), vec![]);
    }

    if clean.starts_with('\'') {
        let parts: Vec<&str> = clean_raw.get(1..).unwrap_or("").splitn(2, ' ').collect();
        if !parts.is_empty() {
            let indices = super::utils::parse_indices(parts[0]);
            let content = parts.get(1).unwrap_or(&"").to_string();
            return (Action::EditAt(scope), content, indices);
        }
    }

    if clean == "-*" {
        return (Action::ClearHistory(scope), String::new(), vec![]);
    }

    if clean.starts_with('-') && clean.len() > 1 {
        let idx_part = &clean[1..];
        let indices = super::utils::parse_indices(idx_part);
        if !indices.is_empty() {
            return (Action::DeleteAt(scope), String::new(), indices);
        }
    }

    (Action::Chat, r.to_string(), vec![])
}

/// `房间?` 后面那个词要落下的状态。
///
/// 返回 `Some(None)` 表示「交回全局配置」，`Some(Some(..))` 是明确的开关，
/// 返回 `None` 表示这个词不认识——调用方据此回一句用法，而不是默默当成切换。
/// 空词是切换：`房间?` 一次就换一边，这是最省事的写法。
pub(crate) fn search_choice(word: &str, current: bool) -> Option<Option<bool>> {
    let word = normalize(word.trim()).trim().to_ascii_lowercase();
    Some(match word.as_str() {
        "" => Some(!current),
        "on" | "开" | "打开" | "联网" | "搜索" | "开联网" => Some(true),
        "off" | "关" | "关闭" | "不联网" | "不搜索" | "关联网" => Some(false),
        "auto" | "默认" | "跟随" | "继承" | "全局" => None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pi_prefixed_rooms_can_be_created_and_use_longest_name() {
        let (name, _, model, _) = parse_create("##pi-test").unwrap();
        assert_eq!(name, "pi-test");
        assert!(model.is_empty());
        assert!(valid_agent_name("PI-猫娘"));
        assert!(!valid_agent_name("pi-../x"));
        assert!(!valid_agent_name("other-x"));
        let rooms = vec!["pi".to_string(), name];
        for input in ["pi-test 你好", "&pi-test 你好", "~pi-test 你好"] {
            let cmd = parse_agent_cmd(input, &rooms).unwrap();
            assert_eq!(cmd.agent, "pi-test");
            assert_eq!(cmd.action, Action::Chat);
        }
        let cmd = parse_agent_cmd("pi-test-1", &rooms).unwrap();
        assert_eq!(cmd.agent, "pi-test");
        assert_eq!(cmd.action, Action::DeleteAt(Scope::Public));
        assert_eq!(cmd.indices, vec![1]);
        assert_eq!(
            parse_delete_agent("-#PI-TEST", &rooms).as_deref(),
            Some("pi-test")
        );
    }

    /// `房间?` 系：一个符号管三件事——切换、明确开关、交回全局。
    #[test]
    fn the_question_mark_room_command_parses_every_spelling() {
        let rooms = vec!["研究".to_string()];
        for (input, expected) in [
            ("研究?", ""),
            ("研究?on", "on"),
            ("研究？开", "开"),
            ("研究? 关", "关"),
            ("研究?auto", "auto"),
        ] {
            let cmd = parse_agent_cmd(input, &rooms).unwrap();
            assert_eq!(cmd.action, Action::SetSearch, "{input}");
            assert_eq!(cmd.args, expected, "{input}");
        }
        // 问号不在房间名后面时，仍然只是普通聊天。
        let cmd = parse_agent_cmd("研究 今天有比赛吗?", &rooms).unwrap();
        assert_eq!(cmd.action, Action::Chat);
        assert_eq!(cmd.args, "今天有比赛吗?");
    }

    #[test]
    fn the_search_word_maps_to_on_off_follow_or_unknown() {
        // 空词是切换：当前关就开，当前开就关。
        assert_eq!(search_choice("", false), Some(Some(true)));
        assert_eq!(search_choice("", true), Some(Some(false)));
        for word in ["on", "ON", "开", "打开", "联网", "搜索"] {
            assert_eq!(search_choice(word, false), Some(Some(true)), "{word}");
        }
        for word in ["off", "OFF", "关", "关闭", "不联网"] {
            assert_eq!(search_choice(word, true), Some(Some(false)), "{word}");
        }
        for word in ["auto", "AUTO", "默认", "跟随", "继承", "全局"] {
            assert_eq!(search_choice(word, true), Some(None), "{word}");
        }
        // 认不出来的词不猜：调用方要回一句用法。
        assert_eq!(search_choice("也许", false), None);
        assert_eq!(search_choice("yes", false), None);
    }
}
