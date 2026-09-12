//! 房间与群聊搭话共用的内置 agent 执行层。
//!
//! 这些房间曾经驱动本机安装的 pi CLI（`pi -p --mode json`），模型、系统提示词与
//! 工具全在那边；代价是部署必须多装一套 Node 工具链，且工具集与提示词都不在自己
//! 手里。现在整条链路就在进程内：一轮对话 = 若干个 Chat Completions 请求，
//! 模型要工具就调用本模块里的实现，把结果作为 tool 消息回填，直到它不再要工具。
//!
//! 三块内容分工：
//! - [`tools`]：工具表（名字、说明、JSON Schema）与本地实现（bash / read / write /
//!   edit / glob / grep），以及转发进 [`super::super::ambient::bridge`] 的 `satori_*`；
//! - [`run`]：消息组装、工具循环、轨迹整理、skill 索引；
//! - [`bash`]：子进程与进程组终止——取消一轮对话必须连带杀掉工具派生出来的进程。
//!
//! 与 pi 时期最大的行为差别是**没有会话文件**：房间历史始终由 ayjx 侧持有，
//! 每轮按需展开成消息，所以编辑/删除/清空/重新生成的行为与普通房间完全一致。

pub(crate) mod bash;
pub(crate) mod run;
pub(crate) mod tools;

pub(crate) use run::run;

use super::types::{ChatMessage, TraceStep};
use std::path::{Path, PathBuf};

/// 旧房间名规则：`pi` 或 `pi-` 前缀（忽略大小写）。
///
/// 引擎现在是房间自己的属性（[`super::types::Agent::uses_pi`]），名字不再决定任何事。
/// 这个函数只剩两个用途：迁移还没写下 `engine` 的旧配置，以及让历史上带 `-`
/// 的房间名继续通过校验。
pub(crate) fn legacy_pi_name(room: &str) -> bool {
    let room = room.trim().to_lowercase();
    room == "pi" || room.starts_with("pi-")
}

/// 解析房间的写法：`pi`、`pi 模型`、`pi/模型`、`pi:模型`（大小写与全角冒号皆可）。
///
/// 返回 `Some(模型)`——空串表示房间没指定模型，用 `[oai] agent_default_model`。
/// 不是这个写法时返回 `None`，调用方按中转站模型处理。
pub(crate) fn parse_pi_spec(spec: &str) -> Option<String> {
    let spec = spec.trim();
    let tail = spec
        .get(..2)
        .filter(|head| head.eq_ignore_ascii_case("pi"))
        .map(|_| &spec[2..])?;
    let mut chars = tail.chars();
    match chars.next() {
        None => Some(String::new()),
        Some('/' | ':' | '：' | ' ' | '\t') => Some(chars.as_str().trim().to_string()),
        // `pixi`、`ping` 这类名字不是这个写法。
        Some(_) => None,
    }
}

/// 房间模型是否表示「用默认模型」：留空或写 `pi`。
///
/// 从前这是「沿用 pi 自身配置」；pi 没了之后由 `[oai] agent_default_model` 接手。
pub(crate) fn uses_default_model(model: &str) -> bool {
    let model = model.trim();
    model.is_empty() || model.eq_ignore_ascii_case("pi")
}

/// 每次调用独占目录，避免中文房间名、私有用户及临时请求之间共享文件。
pub(crate) struct ScratchDir(PathBuf);
impl ScratchDir {
    pub(crate) fn new(base: &Path) -> anyhow::Result<Self> {
        Self::under(base, "runs")
    }

    /// 在 `base/<folder>/` 下开一个只有本进程可读写的临时目录。
    pub(crate) fn under(base: &Path, folder: &str) -> anyhow::Result<Self> {
        let root = base.join(folder);
        std::fs::create_dir_all(&root)?;
        let path = root.join(format!("{:032x}", rand::random::<u128>()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 一次 agent 对话的全部参数。
///
/// 房间与群聊搭话共用同一个执行层：进程组终止、工具循环、事件整理都只该有一份
/// 实现，两个调用方的差别收敛成这里的字段。
pub(crate) struct AgentRun<'a> {
    /// 接口基址（已按供应商解析好）。
    pub api_base: &'a str,
    /// 该接口的密钥。
    pub api_key: &'a str,
    /// 一次调用独占的临时目录：skill 落在这里，bash 的默认工作目录也可是它。
    pub dir: &'a Path,
    /// bash 与文件工具的工作目录；`None` 表示沿用 bot 自身的工作目录。
    pub cwd: Option<&'a Path>,
    /// 整体替换系统提示词；群聊人格不需要通用助手那一套。
    pub system_prompt: Option<&'a str>,
    /// 追加在系统提示词之后的人设。
    pub append_system_prompt: &'a str,
    /// 要发给接口的模型 id（已剥掉 `供应商/` 前缀）。
    pub model: &'a str,
    /// 思考强度（off/minimal/low/…）。
    pub thinking: Option<&'a str>,
    /// 额外载入的 skill 文件或目录。
    pub skills: &'a [PathBuf],
    /// 这一轮持有控制通道凭据：系统提示词里会多一句用法说明。
    pub control: bool,
    /// 工具白名单（逗号分隔）；`None` 用全部本地工具。
    pub tools: Option<&'a str>,
    /// 追加给工具子进程的环境变量，例如本轮对话的控制凭据。
    pub env: &'a [(String, String)],
    /// 单次模型请求的静默上限；`None` 表示不看（交给整轮预算）。
    pub stall: Option<std::time::Duration>,
    /// 有外部动作的会话不能在静默后重放整轮。
    pub retry_stalled: bool,
    /// 真实聊天界面的工具出口；`None` 表示这一轮不接聊天界面。
    pub bridge: Option<std::sync::Arc<super::ambient::bridge::Bridge>>,
    /// 用户正文。
    pub prompt: &'a str,
    /// 随正文送入的图片地址。
    pub images: &'a [String],
    /// 最多几轮工具调用。
    pub max_steps: usize,
}

impl<'a> AgentRun<'a> {
    pub(crate) fn new() -> Self {
        Self {
            api_base: "",
            api_key: "",
            dir: Path::new("."),
            cwd: None,
            system_prompt: None,
            append_system_prompt: "",
            model: "",
            thinking: None,
            skills: &[],
            control: false,
            tools: None,
            env: &[],
            stall: None,
            retry_stalled: true,
            bridge: None,
            prompt: "",
            images: &[],
            max_steps: 24,
        }
    }
}

/// 一次 agent 对话的产出。
#[derive(Debug)]
pub(crate) struct AgentReply {
    pub text: String,
    /// 实际应答的模型（`供应商/模型`），用于回复卡片页脚。
    pub model: Option<String>,
    /// 工具调用轨迹。
    pub trace: Vec<TraceStep>,
    /// 超出保留上限、未进入 `trace` 的调用次数。
    pub trace_overflow: usize,
}

/// 房间对话：按历史展开消息，驱动一轮 agent。
///
/// 房间历史仍是唯一事实来源；中途的工具调用不写回历史，下一轮按历史重新展开。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn conversation(
    api_base: &str,
    api_key: &str,
    base: &Path,
    persona: &str,
    model: &str,
    thinking: Option<&str>,
    stall: Option<std::time::Duration>,
    hist: &[ChatMessage],
    control: Option<&crate::plugins::ctl::bridge::Lease>,
) -> anyhow::Result<AgentReply> {
    let (current, previous) = hist
        .split_last()
        .filter(|(message, _)| message.role == "user")
        .ok_or_else(|| anyhow::anyhow!("没有可重新生成的用户消息，请先发送内容"))?;
    let dir = ScratchDir::new(base)?;
    let env = control.map(|lease| lease.env()).unwrap_or_default();
    let skills: Vec<PathBuf> = control
        .map(|lease| vec![lease.skill().to_path_buf()])
        .unwrap_or_default();
    // 房间的工作目录沿用 bot 自身：与从前一致，bash 与文件工具都在这里活动。
    let cwd = std::env::current_dir().ok();
    run::run_with_history(
        AgentRun {
            api_base,
            api_key,
            dir: dir.path(),
            cwd: cwd.as_deref(),
            append_system_prompt: persona,
            model,
            thinking: thinking.filter(|value| !value.trim().is_empty()),
            skills: &skills,
            control: control.is_some(),
            env: &env,
            stall,
            prompt: &current.content,
            images: &current.images,
            ..AgentRun::new()
        },
        previous,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pi_names_still_recognised_for_migration() {
        assert!(legacy_pi_name("pi"));
        assert!(legacy_pi_name("PI"));
        assert!(legacy_pi_name(" pi "));
        assert!(legacy_pi_name("pi-test"));
        assert!(legacy_pi_name("PI-猫娘"));
        assert!(!legacy_pi_name("ping"));
        assert!(!legacy_pi_name("pixi"));
        assert!(!legacy_pi_name("api"));
        assert!(!legacy_pi_name("pi2"));
        assert!(!legacy_pi_name("助手"));
    }

    #[test]
    fn pi_spec_accepts_every_separator_and_rejects_lookalike_names() {
        assert_eq!(parse_pi_spec("pi").as_deref(), Some(""));
        assert_eq!(parse_pi_spec(" PI ").as_deref(), Some(""));
        for spec in [
            "pi apilio/claude-opus-5",
            "pi/apilio/claude-opus-5",
            "pi:apilio/claude-opus-5",
            "PI：apilio/claude-opus-5",
        ] {
            assert_eq!(
                parse_pi_spec(spec).as_deref(),
                Some("apilio/claude-opus-5"),
                "{spec}"
            );
        }
        // 中转站模型名不能被误当成这种写法。
        for spec in ["", "pixi", "ping", "gpt-5.6-luna", "pi-test", "皮"] {
            assert_eq!(parse_pi_spec(spec), None, "{spec}");
        }
        // 空模型等于「用默认模型」。
        assert!(uses_default_model(&parse_pi_spec("pi").unwrap()));
        assert!(!uses_default_model(&parse_pi_spec("pi kimi-k3").unwrap()));
    }

    #[test]
    fn request_directories_are_unique_and_cleaned() {
        let base = std::env::temp_dir();
        let a = ScratchDir::new(&base).unwrap();
        let b = ScratchDir::new(&base).unwrap();
        assert_ne!(a.path(), b.path());
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }
}
