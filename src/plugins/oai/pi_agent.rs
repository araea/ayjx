//! Pi 房间的本机 pi agent 执行层。
//!
//! 这些房间不再走 OpenAI 端点的工具循环，而是驱动本机安装的 pi CLI
//! （`pi -p --mode json --session <file>`）：模型、系统提示词与工具全部沿用
//! pi 自己的配置，本插件只负责把房间历史灌进会话文件、发起一轮对话、
//! 把 JSON 事件流整理成最终回复与工具轨迹。
//!
//! 会话文件（`pi-sessions/<request>/session.jsonl`）在每次调用前按 ayjx 房间历史整体
//! 重建：房间历史仍是唯一事实来源，编辑/删除/清空/重新生成等指令的行为与
//! 普通房间完全一致；pi 运行后追加的消息（含中间工具调用）下一轮即被覆盖，
//! 不会落进房间历史；每次调用独占目录，结束或取消后清理。
//!
//! 会话文件格式（pi 0.85.x，version 3）：首行 session 头 + 逐条 message 行。
//! assistant 消息必须带 `usage`（pi 加载会话时会统计 token，缺字段直接崩溃），
//! 其余 api/provider/model 字段可省。

use super::types::{ChatMessage, TraceStep};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// 旧的房间名规则：`pi` 或 `pi-` 前缀（忽略大小写）。
///
/// 引擎现在是房间自己的属性（[`super::types::Agent::uses_pi`]），名字不再决定任何事。
/// 这个函数只剩两个用途：迁移还没写下 `engine` 的旧配置，以及让历史上带 `-`
/// 的房间名继续通过校验。
pub(crate) fn legacy_pi_name(room: &str) -> bool {
    let room = room.trim().to_lowercase();
    room == "pi" || room.starts_with("pi-")
}

/// 解析房间的 Pi 写法：`pi`、`pi 模型`、`pi/模型`、`pi:模型`（大小写与全角冒号皆可）。
///
/// 返回 `Some(模型)`——空串表示沿用 pi 自身配置。不是 Pi 写法时返回 `None`，
/// 调用方按中转站模型处理。
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
        // `pixi`、`ping` 这类名字不是 Pi 写法。
        Some(_) => None,
    }
}

/// 每次调用独占目录，避免中文房间名、私有用户及临时请求之间共享文件。
pub(crate) struct ScratchDir(PathBuf);
impl ScratchDir {
    fn new(base: &Path) -> anyhow::Result<Self> {
        Self::under(base, "pi-sessions")
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

/// 房间模型名是否表示「交给 pi 自己决定」。
///
/// pi 房间原本完全不看 ayjx 这边的模型设置，改房间模型毫无效果——这既反直觉，
/// 又让「同一个房间换个模型试试」这种最常见的调试动作没有出口。现在房间模型就是
/// pi 的 `--model`（支持 `provider/id` 与 `id:thinking` 写法）；留空或写 `pi`
/// 表示沿用 pi 自身配置。
pub(crate) fn follows_pi_config(model: &str) -> bool {
    let model = model.trim();
    model.is_empty() || model.eq_ignore_ascii_case("pi")
}

pub(crate) async fn conversation(
    command: &str,
    base: &Path,
    persona: &str,
    model: &str,
    stall: Option<std::time::Duration>,
    hist: &[ChatMessage],
    control: Option<&crate::plugins::ctl::bridge::Lease>,
) -> anyhow::Result<PiReply> {
    let (current, previous) = hist
        .split_last()
        .filter(|(message, _)| message.role == "user")
        .ok_or_else(|| anyhow::anyhow!("没有可重新生成的用户消息，请先发送内容"))?;
    let dir = ScratchDir::new(base)?;
    let session = dir.0.join("session.jsonl");
    sync_session(&session, previous).await?;
    // 控制凭据只在本轮存在：随环境变量交给 pi，并挂上说明用法的 skill。
    let env = control.map(|lease| lease.env()).unwrap_or_default();
    let skills: Vec<PathBuf> = control
        .map(|lease| vec![lease.skill().to_path_buf()])
        .unwrap_or_default();
    run(PiRun {
        session: Some(&session),
        append_system_prompt: persona,
        model: (!follows_pi_config(model)).then_some(model),
        prompt: &current.content,
        images: &current.images,
        skills: &skills,
        env: &env,
        stall,
        ..PiRun::new(command, &dir.0)
    })
    .await
}

/// 一次 pi 对话的产出。
#[derive(Debug)]
pub(crate) struct PiReply {
    pub text: String,
    /// 实际应答的模型（`provider/model`），用于回复卡片页脚。
    pub model: Option<String>,
    /// 工具调用轨迹。
    pub trace: Vec<TraceStep>,
    /// 超出保留上限、未进入 `trace` 的调用次数。
    pub trace_overflow: usize,
}

/// 按房间历史重建 pi 会话文件；历史为空时删除文件让 pi 全新开始。
pub(crate) async fn sync_session(path: &Path, hist: &[ChatMessage]) -> anyhow::Result<()> {
    if hist.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("会话路径缺少父目录"))?;
    std::fs::create_dir_all(dir)?;

    let re = regex::Regex::new(r"!\[.*?\]\((data:image/[^\s\)]+)\)").unwrap();

    let mut lines = String::new();
    lines.push_str(&serde_json::to_string(&json!({
        "type": "session",
        "version": 3,
        "id": format!("ayjx-{}", now_millis()),
        "timestamp": iso_now(),
        "cwd": std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy(),
    }))?);
    lines.push('\n');

    let mut parent_id: Option<String> = None;
    let mut written = 0usize;
    for (index, message) in hist.iter().enumerate() {
        let mut content = Vec::new();
        match message.role.as_str() {
            "user" => {
                if !message.content.trim().is_empty() {
                    content.push(json!({"type": "text", "text": message.content}));
                }
                for url in &message.images {
                    if let Some(part) = image_part(url).await {
                        content.push(part);
                    }
                }
            }
            "assistant" => {
                // 历史里内嵌的 base64 图片重放只会撑爆上下文，留个占位即可。
                let clean = re.replace_all(&message.content, "[Image Created]");
                if clean.trim().is_empty() {
                    continue;
                }
                content.push(json!({"type": "text", "text": clean}));
            }
            _ => continue,
        }
        if content.is_empty() {
            continue;
        }

        let id = format!("ayjx-{index}-{}", now_millis());
        let timestamp = message.timestamp.max(1);
        let mut entry = json!({
            "type": "message",
            "id": id,
            "parentId": parent_id,
            "timestamp": iso_now(),
            "message": {
                "role": message.role,
                "content": content,
                "timestamp": timestamp.saturating_mul(1000),
            },
        });
        if message.role == "assistant" {
            // pi 加载会话时统计 assistant 消息的 usage，缺字段会崩；数值仅供
            // 会话内统计，填 0 不影响模型侧的实际上下文。
            entry["message"]["usage"] = usage_zeros();
            entry["message"]["stopReason"] = json!("stop");
        }
        lines.push_str(&serde_json::to_string(&entry)?);
        lines.push('\n');
        parent_id = Some(id);
        written += 1;
    }

    if written == 0 {
        // 全部消息都被跳过（例如只有空内容的消息），与空历史同义。
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    std::fs::write(path, lines)?;
    Ok(())
}

fn usage_zeros() -> Value {
    json!({
        "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
        "reasoning": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
    })
}

/// 图片地址 → pi 的 image 内容块；无法解析成 data URL（下载失败）时返回 None。
async fn image_part(url: &str) -> Option<Value> {
    let data_url = super::logic::to_data_url(url).await;
    let (mime, data) = data_url.split_once(',')?;
    if !mime.starts_with("data:image/") || !mime.contains("base64") {
        return None;
    }
    let mime = mime
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .unwrap_or("image/png");
    Some(json!({"type": "image", "mimeType": mime, "data": data}))
}

/// 一次 pi 调用的全部参数。
///
/// 房间对话与群聊搭话共用同一个执行层：进程组终止、stdin 传正文、事件流整理
/// 这些都只该有一份实现，两个调用方的差别收敛成这里的字段。
pub(crate) struct PiRun<'a> {
    /// pi 可执行文件或命令。
    pub command: &'a str,
    /// 临时文件（图片）所在目录。
    pub dir: &'a Path,
    /// 会话文件；`None` 表示一次性调用（`--no-session`）。
    pub session: Option<&'a Path>,
    /// 子进程工作目录；`None` 沿用 bot 自身的工作目录。
    pub cwd: Option<&'a Path>,
    /// 追加在 pi 系统提示词之后的人设。
    pub append_system_prompt: &'a str,
    /// 整体替换 pi 的系统提示词；群聊人格不需要编码助手那一套。
    pub system_prompt: Option<&'a str>,
    /// `provider/model`；`None` 用 pi 自己的默认模型。
    pub model: Option<&'a str>,
    /// 思考强度（off/minimal/low/…）。
    pub thinking: Option<&'a str>,
    /// 额外载入的 skill 文件或目录。
    pub skills: &'a [PathBuf],
    /// 显式加载的 Pi 工具扩展。
    pub extensions: &'a [PathBuf],
    /// 有外部动作的会话不能在静默后重放整轮。
    pub retry_stalled: bool,
    /// 工具白名单（逗号分隔）；`None` 用 pi 默认工具集。
    pub tools: Option<&'a str>,
    /// 是否加载工作目录里的 AGENTS.md / CLAUDE.md。
    pub context_files: bool,
    /// 追加给子进程的环境变量，例如本轮对话的控制凭据。
    pub env: &'a [(String, String)],
    /// 事件流静默多久算卡死；`None` 表示不看。
    pub stall: Option<std::time::Duration>,
    /// 用户正文。
    pub prompt: &'a str,
    /// 随正文送入的图片地址。
    pub images: &'a [String],
}

impl<'a> PiRun<'a> {
    pub(crate) fn new(command: &'a str, dir: &'a Path) -> Self {
        Self {
            command,
            dir,
            session: None,
            cwd: None,
            append_system_prompt: "",
            system_prompt: None,
            model: None,
            thinking: None,
            skills: &[],
            extensions: &[],
            retry_stalled: true,
            tools: None,
            context_files: true,
            env: &[],
            stall: None,
            prompt: "",
            images: &[],
        }
    }
}

/// pi 一句话都没说就不动了。
///
/// pi 自身对模型请求不设超时，中转站抽风时它会安静地卡在 epoll 上直到整轮预算耗尽
/// （实测 5 分钟墙钟只烧 2 秒 CPU）。事件流是活着的证据：正常情况下几秒内必有事件，
/// 长时间一个字节都没有只可能是卡死。
#[derive(Debug)]
struct Stalled {
    seconds: u64,
    /// 卡住之前已经产出过正文——重试会让用户等两遍，不如把已有的交出去。
    partial: bool,
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pi 静默超过 {} 秒无任何输出", self.seconds)
    }
}
impl std::error::Error for Stalled {}

/// 运行一轮 pi 对话并整理事件流；卡死且尚未产出正文时自动重来一次。
pub(crate) async fn run(run: PiRun<'_>) -> anyhow::Result<PiReply> {
    let temp = write_images(run.dir, run.images).await?;
    let args = build_args(&run, &temp);
    // 用户正文走 stdin，防止 --model 或 @file 被当作 CLI 参数/文件引用。
    let prompt = if run.prompt.trim().is_empty() {
        "请看图片。"
    } else {
        run.prompt
    };

    let first = attempt(&run, &args, prompt).await;
    match first {
        Err(error) => match error.downcast_ref::<Stalled>() {
            Some(stalled) if !stalled.partial && run.retry_stalled => {
                warn!(target: "Plugin/OAI", "{error}，重试一次");
                attempt(&run, &args, prompt).await
            }
            _ => Err(error),
        },
        ok => ok,
    }
}

fn build_args(run: &PiRun<'_>, temp: &TempFiles) -> Vec<String> {
    let mut args: Vec<String> = vec!["-p".into(), "--mode".into(), "json".into()];
    match run.session {
        Some(session) => {
            args.push("--session".into());
            args.push(session.to_string_lossy().into());
        }
        None => args.push("--no-session".into()),
    }
    if let Some(prompt) = run.system_prompt {
        args.push("--system-prompt".into());
        args.push(prompt.to_string());
    }
    if !run.append_system_prompt.trim().is_empty() {
        args.push("--append-system-prompt".into());
        args.push(format!("房间补充提示：\n{}", run.append_system_prompt));
    }
    if let Some(model) = run.model.filter(|value| !value.trim().is_empty()) {
        args.push("--model".into());
        args.push(model.to_string());
    }
    if let Some(thinking) = run.thinking.filter(|value| !value.trim().is_empty()) {
        args.push("--thinking".into());
        args.push(thinking.to_string());
    }
    for extension in run.extensions {
        args.push("--extension".into());
        args.push(extension.to_string_lossy().into());
    }
    for skill in run.skills {
        args.push("--skill".into());
        args.push(skill.to_string_lossy().into());
    }
    if let Some(tools) = run.tools.filter(|value| !value.trim().is_empty()) {
        args.push("--tools".into());
        args.push(tools.to_string());
    }
    if !run.context_files {
        args.push("--no-context-files".into());
    }
    for path in &temp.0 {
        args.push(format!("@{}", path.display()));
    }
    args
}

/// 起一次 pi 进程，把事件流整理成回复。
async fn attempt(run: &PiRun<'_>, args: &[String], prompt: &str) -> anyhow::Result<PiReply> {
    let command = run.command;

    let mut process = Command::new(command);
    #[cfg(unix)]
    process.process_group(0);
    if let Some(cwd) = run.cwd {
        process.current_dir(cwd);
    }
    for (key, value) in run.env {
        process.env(key, value);
    }
    let mut child = process
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!("无法启动 pi（{command}）：{error}；请检查 [oai].pi_command")
        })?;
    #[cfg(unix)]
    let mut process_group = ProcessGroup {
        pid: child.id().expect("spawned child has a PID") as i32,
        armed: true,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("pi stdout 不可用"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("pi stderr 不可用"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("pi stdin 不可用"))?;
    let input = async {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, anyhow::Error>(())
    };
    let output = async {
        let mut events = Events::default();
        let mut reader = BufReader::new(stdout).lines();
        loop {
            let next = match run.stall {
                Some(limit) => match tokio::time::timeout(limit, reader.next_line()).await {
                    Ok(line) => line?,
                    Err(_) => {
                        return Err(anyhow::Error::new(Stalled {
                            seconds: limit.as_secs(),
                            partial: !events.text.trim().is_empty(),
                        }));
                    }
                },
                None => reader.next_line().await?,
            };
            let Some(line) = next else { break };
            if let Ok(event) = serde_json::from_str::<Value>(&line) {
                events.accept(&event);
            }
        }
        Ok::<_, anyhow::Error>(events)
    };
    // stderr 达到存储上限后仍持续排空，避免进程管道堵塞；取消时三路读取一起释放。
    let errors = async { Ok::<_, anyhow::Error>(read_capped(stderr, 64 * 1024).await) };
    let (_, events, stderr) = tokio::try_join!(input, output, errors)?;
    let status = child.wait().await?;
    #[cfg(unix)]
    {
        process_group.armed = false;
    }
    if !status.success() {
        anyhow::bail!(
            "pi 进程异常退出（{}）：{}",
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    events.finish()
}

/// 取消时结束整个调用树；Pi 的 bash 工具会创建独立进程组。
#[cfg(unix)]
struct ProcessGroup {
    pid: i32,
    armed: bool,
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        kill_descendants(self.pid);
        // SAFETY: 本次 spawn 创建独立进程组，负 PID 不指向 bot 所在组。
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn kill_descendants(pid: i32) {
    // 先暂停父进程，避免遍历时继续产生新子进程。
    // Android 内核可能没有 /proc/PID/task/TID/children，使用 stat 的 PPID。
    // SAFETY: pid 来自本次子进程及其 /proc 父子关系，不对用户输入发送信号。
    if unsafe { libc::kill(pid, libc::SIGSTOP) } != 0 {
        return;
    }
    if let Ok(processes) = std::fs::read_dir("/proc") {
        for process in processes.flatten() {
            let Some(child) = process
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            if child <= 0 || child == pid {
                continue;
            }
            if let Ok(stat) = std::fs::read_to_string(process.path().join("stat")) {
                let parent = stat
                    .rsplit_once(") ")
                    .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                    .and_then(|id| id.parse::<i32>().ok());
                if parent == Some(pid) {
                    kill_descendants(child);
                }
            }
        }
    }
    // SAFETY: 同上，只终止本次请求的子进程。
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// 页脚保留的工具调用条数上限；再多只记次数。
const TRACE_LIMIT: usize = 12;

#[derive(Default)]
struct Events {
    text: String,
    model: Option<String>,
    trace: Vec<TraceStep>,
    trace_overflow: usize,
    error: Option<String>,
}
impl Events {
    fn accept(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("tool_execution_start") => {
                let name = event["toolName"].as_str().unwrap_or("tool");
                let detail = tool_label(event.get("args"));
                // 同一个工具连着用同样的参数（重试、分页）在页脚里排成一列毫无信息量，
                // 合并成一条带次数的记录。
                if let Some(last) = self.trace.last_mut()
                    && last.name == name
                    && last.detail == detail
                {
                    last.repeats += 1;
                } else if self.trace.len() < TRACE_LIMIT {
                    self.trace.push(TraceStep::new(name, detail));
                } else {
                    self.trace_overflow += 1;
                }
            }
            Some("message_end") if event["message"]["role"] == "assistant" => {
                let message = &event["message"];
                self.text.clear();
                self.error = None;
                match message["stopReason"].as_str() {
                    Some("error" | "aborted") => {
                        self.error = Some(
                            message["errorMessage"]
                                .as_str()
                                .unwrap_or("Pi 未完成回复或已中止")
                                .to_string(),
                        );
                    }
                    Some("stop" | "length") => {
                        if let Some(blocks) = message["content"].as_array() {
                            for block in blocks {
                                if block["type"] == "text" {
                                    self.text
                                        .push_str(block["text"].as_str().unwrap_or_default());
                                }
                            }
                        }
                        let provider = message["provider"].as_str().unwrap_or_default();
                        if let Some(model) = message["model"].as_str() {
                            self.model = Some(if provider.is_empty() {
                                model.to_string()
                            } else {
                                format!("{provider}/{model}")
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    fn finish(self) -> anyhow::Result<PiReply> {
        if let Some(error) = self.error {
            anyhow::bail!("pi 运行出错：{error}");
        }
        if self.text.trim().is_empty() {
            anyhow::bail!("pi 未返回最终回复");
        }
        Ok(PiReply {
            text: self.text,
            model: self.model,
            trace: self.trace,
            trace_overflow: self.trace_overflow,
        })
    }
}

/// 工具参数摘要在页脚里的字符上限。
///
/// 页脚按结构渲染、可以换行，所以这里给得比一行文本宽得多；真的超长时也从中间
/// 省略——shell 命令与 URL 的尾巴往往才是分辨「它到底做了什么」的那一段。
const LABEL_LIMIT: usize = 180;

/// 工具参数 → 人类可读的摘要标签（如 `uname -s`、搜索词、URL）。
fn tool_label(args: Option<&Value>) -> String {
    let Some(args) = args.and_then(Value::as_object) else {
        return String::new();
    };
    // serde_json 默认不保序，优先挑常见的关键字段。
    for key in ["command", "query", "url", "pattern", "path", "file"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            return super::utils::truncate_middle(value.trim(), LABEL_LIMIT);
        }
    }
    args.values()
        .find_map(Value::as_str)
        .map(|value| super::utils::truncate_middle(value.trim(), LABEL_LIMIT))
        .unwrap_or_default()
}

/// 下载图片写入临时文件，供 pi 以 `@file` 方式接收；返回 RAII 清理句柄。
async fn write_images(dir: &Path, images: &[String]) -> anyhow::Result<TempFiles> {
    let mut temp = TempFiles::default();
    if images.is_empty() {
        return Ok(temp);
    }
    std::fs::create_dir_all(dir)?;
    use base64::Engine as _;
    for (index, url) in images.iter().enumerate() {
        let part = image_part(url)
            .await
            .ok_or_else(|| anyhow::anyhow!("第 {} 张图片读取失败，请重新发送", index + 1))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(part["data"].as_str().unwrap_or_default())?;
        let ext = match part["mimeType"].as_str() {
            Some("image/jpeg") => "jpg",
            Some("image/gif") => "gif",
            Some("image/webp") => "webp",
            Some("image/bmp") => "bmp",
            _ => "png",
        };
        let path = dir.join(format!("image-{index}.{ext}"));
        std::fs::write(&path, bytes)?;
        temp.0.push(path);
    }
    Ok(temp)
}

/// 临时图片文件句柄；drop 时尽力清理。
#[derive(Default)]
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn read_capped(mut reader: impl AsyncRead + Unpin, max_bytes: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; 4 * 1024];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let remaining = max_bytes.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    kept
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

fn iso_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
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
        // 中转站模型名不能被误当成 Pi 写法。
        for spec in ["", "pixi", "ping", "gpt-5.6-luna", "pi-test", "皮"] {
            assert_eq!(parse_pi_spec(spec), None, "{spec}");
        }
        // 空模型等于「沿用 pi 自身配置」。
        assert!(follows_pi_config(&parse_pi_spec("pi").unwrap()));
        assert!(!follows_pi_config(&parse_pi_spec("pi kimi-k3").unwrap()));
    }

    #[test]
    fn repeated_tool_calls_merge_and_overflow_is_counted() {
        let mut events = Events::default();
        let call = |name: &str, query: &str| json!({"type": "tool_execution_start", "toolName": name, "args": {"query": query}});
        events.accept(&call("web_search", "auto-summary"));
        events.accept(&call("web_search", "auto-summary"));
        events.accept(&call("bash", "uname -a"));
        assert_eq!(events.trace.len(), 2);
        assert_eq!(events.trace[0].repeats, 2);
        assert_eq!(events.trace[1].name, "bash");
        assert_eq!(events.trace[1].detail, "uname -a");

        for index in 0..TRACE_LIMIT + 3 {
            events.accept(&call("web_search", &format!("query-{index}")));
        }
        assert_eq!(events.trace.len(), TRACE_LIMIT);
        assert_eq!(events.trace_overflow, 5);
    }

    #[test]
    fn long_tool_arguments_survive_with_a_middle_ellipsis() {
        let command = format!("bash -lc '{}; echo done'", "a".repeat(400));
        let label = tool_label(Some(&json!({"command": command})));
        assert_eq!(label.chars().count(), LABEL_LIMIT);
        assert!(label.starts_with("bash -lc"), "{label}");
        assert!(label.ends_with("echo done'"), "{label}");
    }

    #[test]
    fn request_directories_are_unique_and_cleaned() {
        let base = std::env::temp_dir();
        let a = ScratchDir::new(&base).unwrap();
        let b = ScratchDir::new(&base).unwrap();
        assert_ne!(a.0, b.0);
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn empty_history_removes_the_session_file() {
        let dir = std::env::temp_dir().join(format!("ayjx-pi-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("room.jsonl");
        std::fs::write(&path, "stale").unwrap();

        sync_session(&path, &[]).await.unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn session_file_follows_pi_jsonl_shape() {
        let dir = std::env::temp_dir().join(format!("ayjx-pi-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("room.jsonl");
        let hist = vec![
            ChatMessage::new("user", "你好", vec![]),
            ChatMessage::new("assistant", "在的", vec![]),
        ];

        sync_session(&path, &hist).await.unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], 3);

        let user: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(user["type"], "message");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(user["message"]["content"][0]["type"], "text");
        assert_eq!(user["message"]["content"][0]["text"], "你好");

        let assistant: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(assistant["message"]["role"], "assistant");
        // pi 加载会话时会读取 assistant 消息的 usage，缺失会直接崩溃。
        assert_eq!(assistant["message"]["usage"]["totalTokens"], 0);
        assert_eq!(assistant["message"]["stopReason"], "stop");
        // parentId 链接到上一条消息。
        assert_eq!(assistant["parentId"], user["id"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn skipped_messages_yield_no_partial_session() {
        let dir = std::env::temp_dir().join(format!("ayjx-pi-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("room.jsonl");
        let hist = vec![ChatMessage::new("assistant", "  ", vec![])];

        sync_session(&path, &hist).await.unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tool_labels_prefer_common_keys() {
        let args = json!({"command": "uname -s", "timeout": 10});
        assert_eq!(tool_label(Some(&args)), "uname -s");
        let long = json!({"command": format!("echo {}", "x".repeat(400))});
        assert_eq!(tool_label(Some(&long)).chars().count(), LABEL_LIMIT);
        assert_eq!(tool_label(None), "");
    }

    #[test]
    fn events_only_return_final_text_and_report_errors_after_tool_use() {
        let mut events = Events::default();
        events.accept(&json!({"type":"tool_execution_start", "toolName":"bash", "args":{"command":"uname -s"}}));
        assert_eq!(events.trace, vec![TraceStep::new("bash", "uname -s")]);
        events.accept(&json!({"type":"message_end", "message":{"role":"assistant", "stopReason":"toolUse", "content":[{"type":"text", "text":"我来检查"}]}}));
        events.accept(
            &json!({"type":"message_end", "message":{"role":"assistant", "stopReason":"error"}}),
        );
        assert!(events.finish().unwrap_err().to_string().contains("未完成"));

        let mut events = Events::default();
        events.accept(&json!({"type":"message_end", "message":{"role":"assistant", "stopReason":"error", "errorMessage":"retry"}}));
        events.accept(&json!({"type":"message_end", "message":{"role":"assistant", "stopReason":"stop", "provider":"local", "model":"test", "content":[{"type":"thinking", "thinking":"secret"},{"type":"text", "text":"最终结果"}]}}));
        let reply = events.finish().unwrap();
        assert_eq!(reply.text, "最终结果");
        assert_eq!(reply.model.as_deref(), Some("local/test"));
    }

    #[cfg(unix)]
    fn fake_pi(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let node = std::process::Command::new("node")
            .args(["-p", "process.execPath"])
            .output()
            .unwrap();
        assert!(node.status.success());
        let path = dir.join("fake-pi");
        std::fs::write(
            &path,
            format!(
                "#!{}\n{}",
                String::from_utf8_lossy(&node.stdout).trim(),
                body
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// 卡死在第一个字节之前：看门狗要掐掉它、重来一次，两次都卡才放弃。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_pi_is_killed_retried_once_and_then_reported() {
        let dir = ScratchDir::new(&std::env::temp_dir()).unwrap();
        let counter = dir.path().join("attempts");
        let command = fake_pi(
            dir.path(),
            &format!(
                "const fs = require('fs');\n\
                 fs.appendFileSync({counter:?}, 'x');\n\
                 setTimeout(() => {{}}, 60000);\n"
            ),
        );

        let error = run(PiRun {
            prompt: "在吗",
            stall: Some(std::time::Duration::from_millis(400)),
            ..PiRun::new(command.to_str().unwrap(), dir.path())
        })
        .await
        .unwrap_err();

        assert!(error.to_string().contains("静默超过"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().len(),
            2,
            "卡死且没出正文时应当恰好重试一次"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_transports_history_images_and_literal_prompt_and_drains_stderr() {
        let dir = ScratchDir::new(&std::env::temp_dir()).unwrap();
        let command = fake_pi(
            &dir.0,
            r#"
const fs = require('fs');
const args = process.argv.slice(2);
let input = '';
process.stdin.on('data', c => input += c);
process.stdin.on('end', () => {
  const session = args[args.indexOf('--session') + 1];
  const history = fs.readFileSync(session, 'utf8').trim().split('\n').map(JSON.parse);
  const img = args.find(x => x.startsWith('@'));
  if (args.includes('--model') || input !== '--model\n@/secret\n用户正文' || history[1].message.content[0].text !== '历史' || history[2].message.usage.totalTokens !== 0 || !img || fs.readFileSync(img.slice(1), 'utf8') !== 'test-image') process.exit(9);
  process.stderr.write('x'.repeat(200000));
  console.log(JSON.stringify({type:'message_end', message:{role:'assistant', stopReason:'stop', model:'fake', content:[{type:'text',text:'传输通过'}]}}));
});
"#,
        );
        let history = vec![
            ChatMessage::new("user", "历史", vec![]),
            ChatMessage::new("assistant", "记住了", vec![]),
            ChatMessage::new(
                "user",
                "--model\n@/secret\n用户正文",
                vec!["data:image/png;base64,dGVzdC1pbWFnZQ==".into()],
            ),
        ];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            conversation(
                command.to_str().unwrap(),
                &dir.0,
                "",
                "",
                None,
                &history,
                None,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.text, "传输通过");
        assert_eq!(
            std::fs::read_dir(dir.0.join("pi-sessions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_pi_and_its_tool_processes_and_cleans_session() {
        let dir = ScratchDir::new(&std::env::temp_dir()).unwrap();
        let marker = dir.0.join("pids.json");
        let command = fake_pi(
            &dir.0,
            &format!(
                r#"
const fs = require('fs');
const child = require('child_process').spawn(process.execPath, ['-e', 'setInterval(()=>{{}},1000)'], {{detached:true}});
fs.writeFileSync({}, JSON.stringify([process.pid,child.pid]));
setInterval(()=>{{}},1000);
"#,
                serde_json::to_string(&marker.to_string_lossy()).unwrap()
            ),
        );
        let history = vec![ChatMessage::new("user", "wait", vec![])];
        let work = tokio::spawn(work_owned(command, dir.0.clone(), history));
        // 等到工具确实启动，再取消，避免只测到启动前的 Future drop。
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        work.abort();
        assert!(work.await.unwrap_err().is_cancelled());
        let pids: Vec<i32> = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let alive = pids.iter().any(|pid| {
                    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                        !stat
                            .split_once(") ")
                            .is_some_and(|(_, fields)| fields.starts_with("Z "))
                    })
                });
                if !alive {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_dir(dir.0.join("pi-sessions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    async fn work_owned(
        command: PathBuf,
        base: PathBuf,
        history: Vec<ChatMessage>,
    ) -> anyhow::Result<PiReply> {
        conversation(
            command.to_str().unwrap(),
            &base,
            "",
            "",
            None,
            &history,
            None,
        )
        .await
    }

    #[tokio::test]
    #[ignore = "调用本机 Pi 和已配置模型，需要网络"]
    async fn live_pi_reads_history_image_and_runs_a_tool() {
        let dir = ScratchDir::new(&std::env::temp_dir()).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0])))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        use base64::Engine as _;
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        );
        let history = vec![
            ChatMessage::new("user", "记住测试暗号：AYJX_PI_MEMORY_83", vec![]),
            ChatMessage::new("assistant", "已记住。", vec![]),
            ChatMessage::new(
                "user",
                "这是接入测试。请执行一次 bash 命令 printf AYJX_PI_TOOL_OK，然后回答：之前的暗号、命令输出、图片是什么颜色。只做这个只读测试，不修改文件。",
                vec![image],
            ),
        ];
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            conversation("pi", &dir.0, "请用中文简短回复。", "", None, &history, None),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(reply.text.contains("AYJX_PI_MEMORY_83"), "{}", reply.text);
        assert!(reply.text.contains("AYJX_PI_TOOL_OK"), "{}", reply.text);
        assert!(reply.text.contains('红'), "{}", reply.text);
        assert!(!reply.trace.is_empty());
        assert!(reply.model.is_some());
        println!(
            "实际模型：{:?}；回复：{}；工具：{:?}",
            reply.model, reply.text, reply.trace
        );
    }
}
