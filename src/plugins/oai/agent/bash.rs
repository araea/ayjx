//! bash 工具：在房间的工作目录里跑一条命令，把合并后的输出交回模型。
//!
//! 这里的边界与从前的 pi 一致——**没有沙箱**。工具本身能做的事 bash 都能做，
//! 真正的边界在「谁能在房间里说话」这一层。留给这里的只有三样：超时、
//! 输出上限，以及取消时把整棵进程树收干净。
//!
//! 进程组终止放在这个文件里是因为它必须紧贴 `Command`：agent 的一轮对话可能被
//! 外层超时或停止指令 drop 掉，`ProcessGroup` 随 future 一起销毁才来得及在那一刻
//! 杀进程；一旦把子进程 `tokio::spawn` 出去，drop 就只是丢掉一个句柄。

use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 交给模型的输出上限（字符）；超出部分从中间省略。
const MAX_OUTPUT: usize = 32_000;
/// 读取阶段的字节上限。比交付上限宽松得多，是为了让「中间省略」能同时留下
/// 命令开头的报错与结尾的结果——边读边砍只会把尾巴整段丢掉。
const MAX_COLLECT: usize = 1024 * 1024;

/// 单条命令的默认与最长时限（秒）。
const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;

/// 跑一条命令；返回合并后的 stdout + stderr。
pub(crate) async fn run(
    command: &str,
    timeout_seconds: Option<u64>,
    cwd: Option<&std::path::Path>,
    env: &[(String, String)],
) -> anyhow::Result<String> {
    let timeout = std::time::Duration::from_secs(
        timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT),
    );

    let mut process = Command::new("bash");
    #[cfg(unix)]
    process.process_group(0);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    for (key, value) in env {
        process.env(key, value);
    }
    let mut child = process
        .args(["-lc", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| anyhow::anyhow!("无法启动 bash：{error}"))?;

    #[cfg(unix)]
    let mut group = ProcessGroup {
        pid: child.id().expect("spawned child has a PID") as i32,
        armed: true,
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("bash stdout 不可用"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("bash stderr 不可用"))?;

    let collect = async {
        // 两路一起排空：只读一路会在另一路写满管道时互相卡死。
        let (out, err) = tokio::join!(
            read_capped(stdout, MAX_COLLECT),
            read_capped(stderr, MAX_COLLECT)
        );
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((out, err, status))
    };

    let (out, err, status) = match tokio::time::timeout(timeout, collect).await {
        Ok(result) => result?,
        Err(_) => {
            // 超时即放弃这一轮：进程组连同 bash 派生的独立进程一起收掉。
            return Err(anyhow::anyhow!(
                "命令超过 {} 秒未结束，已终止",
                timeout.as_secs()
            ));
        }
    };
    #[cfg(unix)]
    {
        group.armed = false;
    }

    let mut text = String::from_utf8_lossy(&out).into_owned();
    let err = String::from_utf8_lossy(&err);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    let text = super::super::utils::truncate_middle(text.trim_end(), MAX_OUTPUT);
    if !status.success() {
        // 非零退出把状态写清楚再交回去：命令失败本身也是有用的结果，模型据此改做法。
        anyhow::bail!(
            "命令以 {} 退出{}",
            status.code().unwrap_or(-1),
            if text.is_empty() {
                String::new()
            } else {
                format!("：\n{text}")
            }
        );
    }
    if text.is_empty() {
        return Ok("（命令没有输出）".to_string());
    }
    Ok(text)
}

/// 取消时结束整个调用树；bash 会为管道与后台任务创建独立进程组。
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

async fn read_capped(mut reader: impl tokio::io::AsyncRead + Unpin, max_bytes: usize) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_and_exit_status_reach_the_caller() {
        let out = run("printf '甲'; printf '乙' 1>&2", None, None, &[])
            .await
            .unwrap();
        assert!(out.contains('甲') && out.contains('乙'), "{out}");

        let error = run("echo boom 1>&2; exit 3", None, None, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("以 3 退出"), "{error}");
        assert!(error.contains("boom"), "{error}");
    }

    #[tokio::test]
    async fn extra_env_and_working_directory_are_honoured() {
        let dir = std::env::temp_dir();
        let out = run(
            "printf '%s' \"$AYJX_TEST_MARKER\"; pwd -P",
            None,
            Some(&dir),
            &[("AYJX_TEST_MARKER".to_string(), "已注入".to_string())],
        )
        .await
        .unwrap();
        assert!(out.starts_with("已注入"), "{out}");
        assert!(out.contains(dir.file_name().unwrap().to_str().unwrap()), "{out}");
    }

    #[tokio::test]
    async fn a_hung_command_is_killed_at_the_deadline() {
        let started = std::time::Instant::now();
        let error = run("sleep 30", Some(1), None, &[]).await.unwrap_err().to_string();
        assert!(error.contains("已终止"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    /// 取消一轮对话必须连带收掉 bash 派生出来的进程，否则每按一次「停止」都会漏一个。
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_whole_process_tree() {
        let dir = std::env::temp_dir().join(format!("ayjx-bash-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("pids");
        let target = marker.clone();
        let work = tokio::spawn(async move {
            run(
                &format!(
                    "bash -c 'bash -c \"echo $$; sleep 60\" & wait' > {}",
                    target.display()
                ),
                Some(60),
                None,
                &[],
            )
            .await
        });

        // 等孙子进程真的起来了再取消——太早只会测到启动前的 drop。
        let pids = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&marker)
                    && let Some(pid) = text.split_whitespace().next()
                {
                    break pid.to_string();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        work.abort();
        assert!(work.await.unwrap_err().is_cancelled());

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let alive = std::fs::read_to_string(format!("/proc/{pids}/stat")).is_ok_and(|stat| {
                    !stat
                        .split_once(") ")
                        .is_some_and(|(_, fields)| fields.starts_with("Z "))
                });
                if !alive {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn long_output_keeps_both_ends() {
        let out = run(
            "head -c 200000 /dev/zero | tr '\\0' 'a'; printf 'THE-END'",
            None,
            None,
            &[],
        )
        .await
        .unwrap();
        assert!(out.ends_with("THE-END"), "尾巴必须留下");
        assert!(out.chars().count() <= MAX_OUTPUT, "{}", out.chars().count());
    }
}
