//! 一台够用的 HTTP/1.1 服务端。
//!
//! 面板要服务的是「同一部手机上的一个浏览器标签页」：一份静态页面、五个 JSON
//! 接口、峰值并发个位数。为此引入一整套 web 框架，换来的是几分钟的编译时间和
//! 一串新依赖，省下的不过是本文件这两百行——所以这里自己解析请求行、头部与
//! 定长 body，其余一概不做。
//!
//! 边界写死在三处：请求头 16 KiB、body 256 KiB、单次会话 20 秒。任何一项越界就
//! 断开——这台服务端跑在机器人进程里，它的首要义务是别把机器人拖垮。
//!
//! 每个响应都带 `Connection: close`。少了长连接的复用，但也少了一整类状态；
//! 本机往返的开销远小于维护它的代价。

use super::api;
use serde_json::{Value as Json, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 面板页面。整页内联，不取任何外部资源——这台机器随时可能没有网。
const INDEX: &str = include_str!("../../../res/webui/index.html");

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 256 * 1024;
const SESSION_TIMEOUT: Duration = Duration::from_secs(20);
/// 密钥错误时的固定延迟。本机面板谈不上什么攻击面，但把在线穷举变成一件
/// 慢得没有意义的事，代价只是三百毫秒。
const AUTH_DELAY: Duration = Duration::from_millis(300);

pub(super) async fn serve(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(SESSION_TIMEOUT, session(stream)).await;
                });
            }
            Err(error) => {
                warn!(target: super::LOG_TARGET, "面板监听出错：{error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    token: Option<String>,
    body: Vec<u8>,
}

async fn session(mut stream: TcpStream) -> std::io::Result<()> {
    let request = match read_request(&mut stream).await? {
        Some(request) => request,
        None => return write_response(&mut stream, 400, "text/plain; charset=utf-8", b"bad request").await,
    };
    let (status, kind, body) = route(request).await;
    write_response(&mut stream, status, kind, &body).await
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_HEAD {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let method = start.next().unwrap_or_default().to_string();
    let target = start.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        return Ok(None);
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut length = 0usize;
    let mut token = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "authorization" => {
                token = value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .map(str::to_string);
            }
            "x-ayjx-token" => token = Some(value.to_string()),
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Ok(None);
    }

    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);

    Ok(Some(Request {
        method,
        path,
        query,
        token,
        body,
    }))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 常量时间比较：长度不同直接判负，长度相同时不提前退出。
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn authorized(request: &Request) -> bool {
    let Some(running) = super::running() else {
        return false;
    };
    let expected =
        crate::plugins::get_config_or_default::<super::Config>(&running.ctx, "webui").token;
    // 密钥也允许写在查询串里：从日志里复制出来的那条链接必须一次就能打开。
    let supplied = request.token.clone().or_else(|| {
        request
            .query
            .split('&')
            .find_map(|pair| pair.strip_prefix("k=").map(str::to_string))
    });
    supplied.is_some_and(|given| secret_eq(&given, &expected))
}

async fn route(request: Request) -> (u16, &'static str, Vec<u8>) {
    const HTML: &str = "text/html; charset=utf-8";
    const JSON: &str = "application/json; charset=utf-8";

    match (request.method.as_str(), request.path.as_str()) {
        // 页面本身不含任何配置，凭密钥取数据的是它里面的脚本。
        ("GET", "/") | ("GET", "/index.html") => (200, HTML, INDEX.as_bytes().to_vec()),
        ("GET", "/favicon.ico") => (204, "image/x-icon", Vec::new()),
        ("GET" | "POST", path) if path.starts_with("/api/") => {
            if !authorized(&request) {
                tokio::time::sleep(AUTH_DELAY).await;
                return (
                    401,
                    JSON,
                    body(&json!({"ok": false, "message": "密钥无效；请用日志里的链接重新打开面板。"})),
                );
            }
            let Some(running) = super::running() else {
                return (
                    503,
                    JSON,
                    body(&json!({"ok": false, "message": "面板尚未就绪"})),
                );
            };
            api_route(&running.ctx, &request).await
        }
        ("OPTIONS", _) => (204, "text/plain", Vec::new()),
        _ => (404, "text/plain; charset=utf-8", b"not found".to_vec()),
    }
}

async fn api_route(
    ctx: &crate::event::Context,
    request: &Request,
) -> (u16, &'static str, Vec<u8>) {
    const JSON: &str = "application/json; charset=utf-8";
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/version") => (
            200,
            JSON,
            body(&json!({"ok": true, "version": api::version(ctx)})),
        ),
        ("GET", "/api/state") => (
            200,
            JSON,
            body(&json!({"ok": true, "state": api::state(ctx)})),
        ),
        ("POST", "/api/command") => match serde_json::from_slice::<api::Command>(&request.body) {
            Ok(command) => {
                let (ok, message, state) = api::execute(ctx, command).await;
                (
                    200,
                    JSON,
                    body(&json!({"ok": ok, "message": message, "state": state})),
                )
            }
            Err(error) => (
                400,
                JSON,
                body(&json!({"ok": false, "message": format!("请求格式错误：{error}")})),
            ),
        },
        _ => (
            404,
            JSON,
            body(&json!({"ok": false, "message": "接口不存在"})),
        ),
    }
}

fn body(value: &Json) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec())
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    kind: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "OK",
    };
    // 页面与接口都不该被缓存：配置改完刷新就得是新的。
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {kind}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_comparison_rejects_length_and_content_mismatch() {
        assert!(secret_eq("abcdef", "abcdef"));
        assert!(!secret_eq("abcdef", "abcdeg"));
        assert!(!secret_eq("abcdef", "abcde"));
        assert!(!secret_eq("", ""), "空密钥永远不算通过");
    }

    #[tokio::test]
    async fn requests_are_parsed_and_oversized_bodies_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let parsed = read_request(&mut stream).await.unwrap();
            let (mut stream2, _) = listener.accept().await.unwrap();
            let rejected = read_request(&mut stream2).await.unwrap();
            (parsed, rejected.is_none())
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"POST /api/command?k=key HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\nContent-Length: 2\r\n\r\n{}",
            )
            .await
            .unwrap();
        client.flush().await.unwrap();

        let mut oversized = TcpStream::connect(addr).await.unwrap();
        oversized
            .write_all(
                format!("POST /api/command HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1)
                    .as_bytes(),
            )
            .await
            .unwrap();
        oversized.flush().await.unwrap();

        let (parsed, refused) = server.await.unwrap();
        let parsed = parsed.expect("第一条请求应当解析成功");
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/api/command");
        assert_eq!(parsed.query, "k=key");
        assert_eq!(parsed.token.as_deref(), Some("tok"));
        assert_eq!(parsed.body, b"{}");
        assert!(refused, "超长 body 必须被拒绝");
    }

    /// 页面不该带上任何外链，否则断网的机器上打开就是一张白纸。
    #[test]
    fn the_page_is_self_contained() {
        assert!(!INDEX.contains("http://"), "页面出现外部链接");
        assert!(!INDEX.contains("https://"), "页面出现外部链接");
        assert!(INDEX.contains("<script"), "页面缺少脚本");
    }
}
