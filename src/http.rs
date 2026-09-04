//! 统一的 HTTP 客户端。
//!
//! reqwest 的 `rustls` 特性走 `rustls-platform-verifier`，它在 Android 上要靠
//! JVM 侧的 `init_hosted` 注入证书存储。Termux 里跑的是纯 CLI 进程，没有 JVM，
//! 首次 TLS 握手会直接 panic（`Expect rustls-platform-verifier to be
//! initialized`），且 panic 发生在 tokio 工作线程里——插件表现为「没反应」。
//!
//! 所以 Android 上改从系统 CA 包装载根证书，让 rustls 走纯 webpki 校验；
//! 其他平台保持 reqwest 默认行为。

use reqwest::{Certificate, Client, ClientBuilder};
use std::sync::OnceLock;

/// Termux 与常见发行版的 CA 包位置；`SSL_CERT_FILE` 优先。
const CA_BUNDLES: &[&str] = &[
    "/data/data/com.termux/files/usr/etc/tls/cert.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/ssl/cert.pem",
    "/etc/pki/tls/certs/ca-bundle.crt",
];

#[cfg(target_os = "android")]
fn ca_bundle() -> Option<&'static [u8]> {
    static BUNDLE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            std::env::var("SSL_CERT_FILE")
                .ok()
                .into_iter()
                .chain(CA_BUNDLES.iter().map(|path| (*path).to_string()))
                .find_map(|path| std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty()))
        })
        .as_deref()
}

/// 带正确根证书配置的客户端构建器，供需要自定超时/UA 的调用方使用。
pub fn builder() -> ClientBuilder {
    let builder = Client::builder();
    #[cfg(target_os = "android")]
    {
        if let Some(certs) = ca_bundle().and_then(|pem| Certificate::from_pem_bundle(pem).ok())
            && !certs.is_empty()
        {
            return builder.tls_certs_only(certs);
        }
        warn!(target: "System", "未找到系统 CA 包，HTTPS 请求可能失败");
    }
    builder
}

/// 全局共享客户端（内部为 Arc，clone 只是增加引用计数）。
pub fn client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| builder().build().unwrap_or_default())
        .clone()
}

/// `reqwest::get` 的替代：共享连接池，且带正确的根证书。
pub async fn get<U: reqwest::IntoUrl>(url: U) -> reqwest::Result<reqwest::Response> {
    client().get(url).send().await
}

/// 下载资源到内存（图片等小体积文件用）。
pub async fn download_bytes(url: &str) -> reqwest::Result<Vec<u8>> {
    let resp = get(url).await?;
    let bytes = resp.error_for_status()?.bytes().await?;
    Ok(bytes.to_vec())
}
