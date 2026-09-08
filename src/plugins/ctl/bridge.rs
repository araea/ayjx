//! pi 房间 → ctl 的本机控制通道。
//!
//! pi 房间里的 agent 本来就有一个全权限 shell，所以「让它改机器人配置」的难点从来
//! 不是能力，而是**怎么改**：直接去写 `config.toml` 会被运行中的实例在退出时覆盖，
//! 而绕道控制台（`tmux send-keys`）拿不到任何回执，agent 只能盲发。
//!
//! 这里给出一条正路：一轮 pi 房间对话开始时签发一枚一次性凭据，随环境变量交给 pi
//! 子进程；agent 用 `ayjx --ctl "<命令>"` 把命令送回本进程的 Unix 套接字，由
//! [`super::execute`] 以维护者身份执行，再把 `/ctl` 原样的回执打回 stdout——
//! 于是 agent 能看见结果并据此继续。
//!
//! **这条通道不做身份限制**：任何能在 pi 房间里说话的人都能借它操作机器人，这是
//! 部署者明确的选择（`[ctl].pi_control`，默认开）。它并没有扩大 pi 房间的能力边界
//! ——同一个 agent 手里的 bash 能做的事只多不少——但确实把「改配置」从管理员专属
//! 变成了群友可用。要收回这份信任有两个层次：关掉本开关只堵住这条通道，真正的边界
//! 在 pi 侧的工具配置。
//!
//! 留下的两样东西不是权限，是可恢复性：每条命令都按「控制通道执行」记进日志，
//! 凭据随这一轮对话作废、不落盘、不上命令行。

use crate::event::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const LOG_TARGET: &str = "Plugin/Ctl";

/// 凭据的兜底寿命。正常情况下随 [`Lease`] 释放，这里只防「进程没走 Drop」的极端情况。
const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// 单条请求的长度上限；命令是一行文字，不需要更多。
const MAX_REQUEST: u64 = 64 * 1024;

/// 描述控制命令用法的 skill；`{{AYJX}}` 在落盘时替换成真实可执行文件路径。
const SKILL: &str = include_str!("../../../res/ctl/skills/ayjx-control/SKILL.md");

/// 一枚有效凭据：以什么上下文执行、谁触发的、什么时候过期。
struct Grant {
    ctx: Context,
    /// 触发这一轮对话的 QQ 号，只用于日志追溯。
    user: i64,
    expires: Instant,
}

fn grants() -> &'static Mutex<HashMap<String, Grant>> {
    static GRANTS: OnceLock<Mutex<HashMap<String, Grant>>> = OnceLock::new();
    GRANTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Grant>> {
    grants().lock().unwrap_or_else(|error| error.into_inner())
}

/// 一轮对话的控制授权；drop 即作废。
pub(crate) struct Lease {
    token: String,
    socket: PathBuf,
    binary: PathBuf,
    skill: PathBuf,
}

impl Lease {
    /// 交给 pi 子进程的环境变量。
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![
            ("AYJX_CTL_TOKEN".to_string(), self.token.clone()),
            (
                "AYJX_CTL_SOCK".to_string(),
                self.socket.to_string_lossy().into_owned(),
            ),
            (
                "AYJX_CTL_BIN".to_string(),
                self.binary.to_string_lossy().into_owned(),
            ),
        ]
    }

    /// 说明控制命令用法的 skill 目录。
    pub(crate) fn skill(&self) -> &Path {
        &self.skill
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        lock().remove(&self.token);
    }
}

/// 为一轮 pi 房间对话签发控制凭据。
///
/// `[ctl].pi_control` 关闭或套接字起不来时返回 `None`——那时 pi 房间的行为与从前
/// 完全一致。除此之外不看发起人是谁：这条通道对所有人开放。
pub(crate) async fn lease(ctx: &Context) -> Option<Lease> {
    if !enabled(ctx) {
        return None;
    }
    let user = ctx.as_message().map(|event| event.user_id()).unwrap_or(0);
    let (socket, skill) = match ensure_server().await {
        Ok(paths) => paths,
        Err(error) => {
            warn!(target: LOG_TARGET, "控制通道未能启动：{error}");
            return None;
        }
    };
    let token = format!("{:032x}", rand::random::<u128>());
    lock().insert(
        token.clone(),
        Grant {
            ctx: maintainer(ctx),
            user,
            expires: Instant::now() + MAX_LIFETIME,
        },
    );
    Some(Lease {
        token,
        socket,
        binary: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ayjx")),
        skill,
    })
}

fn enabled(ctx: &Context) -> bool {
    crate::plugins::get_config_or_default::<super::Config>(ctx, "ctl").pi_control
}

/// 把上下文换成维护者身份执行。
///
/// ctl 内部按 `is_manager` 判权，而本机控制台一向等同维护者；这条通道既然对所有人
/// 开放，就走同一条身份，免得同一份命令在两处得到两种结果。ctl 自己的保命规则
/// （不能关掉 ctl、不能把管理入口锁死）仍然生效，它们防的是误操作而不是权限。
fn maintainer(ctx: &Context) -> Context {
    let mut ctx = ctx.clone();
    ctx.bot = std::sync::Arc::new(crate::event::BotStatus {
        adapter: "console".to_string(),
        platform: "console".to_string(),
        login_user: ctx.bot.login_user.clone(),
    });
    ctx
}

/// 起一次监听并铺开 skill；返回（套接字路径，skill 目录）。
async fn ensure_server() -> anyhow::Result<(PathBuf, PathBuf)> {
    // 两轮对话同时开场时只能有一个人去 bind：另一个若中途把套接字文件删掉重建，
    // 先起来的那个监听器就成了没有名字的孤儿。用异步 OnceCell 把初始化串起来。
    static READY: tokio::sync::OnceCell<(PathBuf, PathBuf)> = tokio::sync::OnceCell::const_new();
    READY.get_or_try_init(start_server).await.cloned()
}

async fn start_server() -> anyhow::Result<(PathBuf, PathBuf)> {
    let dir = crate::plugins::get_data_dir("ctl")
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let socket = dir.join("control.sock");
    let skill = write_skill(&dir).await?;

    // 上一次运行留下的套接字文件会让 bind 失败；此刻没有第二个实例在跑（单实例锁由
    // 启动脚本保证），直接清掉。
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        serve(listener).await;
    });
    info!(target: LOG_TARGET, "控制通道已就绪：{}", socket.display());
    Ok((socket, skill))
}

/// 把 skill 写进数据目录，并把可执行文件的真实路径嵌进去。
async fn write_skill(dir: &Path) -> anyhow::Result<PathBuf> {
    let binary = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "ayjx".to_string());
    let skill = dir.join("skills/ayjx-control");
    tokio::fs::create_dir_all(&skill).await?;
    tokio::fs::write(
        skill.join("SKILL.md"),
        SKILL.replace("{{AYJX}}", &binary),
    )
    .await?;
    Ok(skill)
}

async fn serve(listener: tokio::net::UnixListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            if let Err(error) = session(stream).await {
                warn!(target: LOG_TARGET, "控制通道请求失败：{error}");
            }
        });
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    command: String,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    text: String,
}

async fn session(mut stream: tokio::net::UnixStream) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = stream.split();
    let mut line = String::new();
    BufReader::new(read.take(MAX_REQUEST))
        .read_line(&mut line)
        .await?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handle(request).await,
        Err(error) => Response {
            ok: false,
            text: format!("请求格式错误：{error}"),
        },
    };
    write
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;
    write.flush().await?;
    Ok(())
}

async fn handle(request: Request) -> Response {
    let now = Instant::now();
    let grant = {
        let mut grants = lock();
        grants.retain(|_, grant| grant.expires > now);
        grants
            .get(&request.token)
            .map(|grant| (grant.ctx.clone(), grant.user))
    };
    let Some((ctx, user)) = grant else {
        // 凭据过期或伪造：不透露任何配置，也不区分两者。
        warn!(target: LOG_TARGET, "控制通道拒绝了一个无效凭据");
        return Response {
            ok: false,
            text: "凭据无效或已过期：控制授权只在发起它的那一轮对话内有效。".to_string(),
        };
    };
    let command = request.command.trim().to_string();
    // 审计：谁、在哪一轮、执行了什么，落进与聊天同一份日志。
    info!(target: LOG_TARGET, "控制通道执行（{user}）：{command}");
    match super::execute(&ctx, &command).await {
        Ok(text) => Response { ok: true, text },
        Err(text) => Response {
            ok: false,
            text: format!("操作未完成：{text}"),
        },
    }
}

/// `ayjx --ctl "<命令>"`：把命令送进正在运行的实例并打印回执。
///
/// 只读环境变量，不接受套接字或凭据参数——凭据出现在命令行上就会进 `ps` 和 shell
/// 历史，而这条通道的全部安全性都系在它身上。
pub fn client(command: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let socket = std::env::var("AYJX_CTL_SOCK")
        .map_err(|_| "缺少 AYJX_CTL_SOCK：控制通道只在 ayjx 的 pi 房间对话内可用。")?;
    let token = std::env::var("AYJX_CTL_TOKEN")
        .map_err(|_| "缺少 AYJX_CTL_TOKEN：本轮对话没有获得控制授权。")?;

    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| format!("无法连接控制通道（{socket}）：{error}"))?;
    let request = serde_json::to_string(&Request {
        token,
        command: command.to_string(),
    })
    .map_err(|error| error.to_string())?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .and_then(|_| stream.flush())
        .and_then(|_| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|error| format!("发送失败：{error}"))?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("读取回执失败：{error}"))?;
    let response: Response =
        serde_json::from_str(&line).map_err(|error| format!("回执解析失败：{error}"))?;
    if response.ok {
        Ok(response.text)
    } else {
        Err(response.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::event::{BotStatus, EventType, LoginUser};
    use crate::matcher::Matcher;
    use crate::scheduler::Scheduler;
    use sea_orm::Database;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex as AsyncMutex;

    /// 一个「发起人是 QQ 用户」的上下文；`admin` 决定这个人在不在 ctl.admins 里。
    async fn context(admin: bool) -> Context {
        let mut config = AppConfig::default();
        for plugin in crate::plugins::get_plugins() {
            config
                .plugins
                .insert(plugin.name.into(), (plugin.default_config)());
        }
        if admin
            && let Some(table) = config.plugins.get_mut("ctl").and_then(toml::Value::as_table_mut)
        {
            table.insert("admins".into(), toml::Value::Array(vec![42.into()]));
        }
        let path =
            std::env::temp_dir().join(format!("ayjx-bridge-test-{}.toml", rand::random::<u64>()));
        Context {
            event: EventType::Satori(
                simd_json::serde::to_owned_value(serde_json::json!({
                    "post_type": "message", "message_type": "private",
                    "user_id": 42, "message_id": 1, "raw_message": "pi 看看插件",
                    "message": [{"type": "text", "data": {"text": "pi 看看插件"}}]
                }))
                .unwrap(),
            ),
            config: Arc::new(RwLock::new(config)),
            config_save_lock: Arc::new(AsyncMutex::new(())),
            db: Database::connect("sqlite::memory:").await.unwrap(),
            scheduler: Arc::new(Scheduler::new()),
            matcher: Arc::new(Matcher::new()),
            config_path: Arc::from(path.to_str().unwrap()),
            bot: Arc::new(BotStatus {
                adapter: "satori-qq".into(),
                platform: "qq".into(),
                login_user: LoginUser::default(),
            }),
        }
    }

    /// 直接走套接字，不经环境变量——测的是服务端而不是客户端的取值方式。
    async fn ask(socket: &Path, token: &str, command: &str) -> Response {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let request = serde_json::to_string(&Request {
            token: token.to_string(),
            command: command.to_string(),
        })
        .unwrap();
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// 监听器活在创建它的那个 runtime 里，所以这条通道的全部行为放在同一个
    /// `#[tokio::test]` 里验证：对谁都签发、能执行、凭据作废。
    #[tokio::test]
    async fn the_channel_is_open_to_everyone_and_expires_with_the_turn() {
        // 不在 ctl.admins 里的普通群友同样拿得到凭据——这条通道不做身份限制。
        let ctx = context(false).await;
        let lease = lease(&ctx).await.expect("任何 pi 房间对话都应当拿到凭据");
        let socket = lease.socket.clone();
        let token = lease.token.clone();

        let listed = ask(&socket, &token, "list").await;
        assert!(listed.ok, "{}", listed.text);
        assert!(listed.text.contains("oai"), "{}", listed.text);

        // 写操作同样放行：执行身份是维护者，而不是发起人自己的 QQ 身份。
        let changed = ask(&socket, &token, "off help").await;
        assert!(changed.ok, "{}", changed.text);
        assert!(
            !super::super::enabled(&ctx.config.read().unwrap(), "help"),
            "写操作应当真的落到内存配置上"
        );

        let denied = ask(&socket, &"0".repeat(32), "list").await;
        assert!(!denied.ok);
        assert!(denied.text.contains("凭据无效"), "{}", denied.text);

        // ctl 的保命规则不受影响：它防的是把自己锁在门外，不是权限。
        let refused = ask(&socket, &token, "off ctl").await;
        assert!(!refused.ok, "{}", refused.text);

        drop(lease);
        let expired = ask(&socket, &token, "list").await;
        assert!(!expired.ok, "本轮结束后凭据必须立即失效：{}", expired.text);
    }

    #[test]
    fn skill_carries_the_real_binary_path() {
        assert!(SKILL.contains("{{AYJX}}"), "skill 必须留占位符供落盘时替换");
        let rendered = SKILL.replace("{{AYJX}}", "/opt/ayjx");
        assert!(rendered.contains(r#""/opt/ayjx" --ctl"#), "{rendered}");
        assert!(!rendered.contains("{{AYJX}}"));
    }

    #[test]
    fn client_refuses_to_run_outside_a_granted_turn() {
        // SAFETY: 单线程测试内改自身环境变量。
        unsafe {
            std::env::remove_var("AYJX_CTL_SOCK");
            std::env::remove_var("AYJX_CTL_TOKEN");
        }
        assert!(client("list").unwrap_err().contains("AYJX_CTL_SOCK"));
    }
}
