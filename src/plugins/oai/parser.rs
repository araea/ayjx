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

    if name.is_empty()
        || name.chars().count() > 7
        || name.chars().any(|c| "&\"#~/ -_'!@$%:*".contains(c))
    {
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
    if agents.iter().any(|a| a.eq_ignore_ascii_case(name)) {
        Some(name.to_string())
    } else {
        None
    }
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
