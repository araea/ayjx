use crate::adapters::satori::{LockedWriter, send_msg};
use crate::command::find_url;
use crate::config::build_config;
use crate::event::Context;
use crate::message::Message;
use crate::plugins::{ChannelConfig, PluginError, get_config_or_default};
use anyhow::{Result, anyhow};
use cdp_html_shot::{Browser, CaptureOptions, ImageFormat, Viewport};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsArray, ValueObjectAccessAsScalar};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time;
use toml::Value;
use url::{Host, Url};

// ================= Config =================

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub max_height: u32,
    pub timeout_seconds: u64,
    pub quality: u8,
    pub viewport_width: u32,
    pub device_scale_factor: f64,
    pub ignore_domains: Vec<String>,
    /// 是否允许截图访问内网/本机地址。默认关闭——见 `check_url` 的说明。
    pub allow_private_hosts: bool,
    /// 群名单：配了黑名单就对名单外的所有群截图，配了白名单则只对名单内的群截图。
    pub channel: ChannelConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            max_height: 5000,
            timeout_seconds: 30,
            quality: 80,
            viewport_width: 1280,
            device_scale_factor: 1.0,
            ignore_domains: vec![],
            allow_private_hosts: false,
            channel: ChannelConfig::default(),
        }
    }
}

pub fn default_config() -> Value {
    build_config(Config::default())
}

// ================= 链接准入 =================

/// 同时进行的网页截图上限。任意群友都能用一条链接触发渲染，没有闸门时
/// 大量页面会同时吃内存——本机的卡片截图因此同样是串行的。
static CAPTURE_GATE: Semaphore = Semaphore::const_new(2);

/// 单张截图的像素上限（含 `device_scale_factor`），与 `render/web.rs` 保持一致。
const MAX_CAPTURE_PIXELS: f64 = 64_000_000.0;

/// 链接是否允许截图，不允许时返回可写进日志的原因。
///
/// 机器人跑在本机，`[webshot]` 又把截图原样发回群里，所以任意群友都能借一条链接
/// 把 `127.0.0.1:6520` 的控制面板、`192.168.x.x` 的路由器后台渲染成图片读走。
/// 这里在交给浏览器之前先把主机拦下来，并用 `url` 做规范化，`http://0x7f000001/`、
/// `http://2130706433/` 这类写法会先被还原成 `127.0.0.1` 再判定。
async fn check_url(raw: &str, config: &Config) -> std::result::Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "无法解析的链接".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("不支持的协议 {}", url.scheme()));
    }
    let host = url.host().ok_or_else(|| "链接缺少主机名".to_string())?;

    if let Host::Domain(name) = host {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        if let Some(rule) = config
            .ignore_domains
            .iter()
            .find(|rule| domain_matches(&name, rule))
        {
            return Err(format!("域名 {} 命中忽略名单 {}", name, rule.trim()));
        }
    }

    if !config.allow_private_hosts && host_is_internal(&host).await {
        return Err(format!("{} 指向内网/本机地址", url));
    }

    Ok(url)
}

/// 忽略名单按域名后缀匹配：`evil.com` 同时覆盖 `a.evil.com`，
/// 但不会像子串匹配那样把 `evil.com.attacker.net` 也算进去。
fn domain_matches(host: &str, rule: &str) -> bool {
    let rule = rule.trim().trim_start_matches('.').to_ascii_lowercase();
    !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}")))
}

/// 主机是否落在不该被截图访问的地址上。
///
/// IP 字面量直接判定；域名会真实解析一次，所以 `127.0.0.1.nip.io` 这类
/// 「公网域名解析回本机」的绕过同样会被拦下。
async fn host_is_internal(host: &Host<&str>) -> bool {
    match host {
        Host::Ipv4(ip) => ip_is_internal(IpAddr::V4(*ip)),
        Host::Ipv6(ip) => ip_is_internal(IpAddr::V6(*ip)),
        Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            if name == "localhost"
                || name.ends_with(".localhost")
                || name.ends_with(".local")
                || name.ends_with(".internal")
                || name.ends_with(".lan")
                || name.ends_with(".home.arpa")
            {
                return true;
            }
            // `ToSocketAddrs` 会阻塞，挪到阻塞线程；解析失败时放行，
            // 让浏览器自己失败，免得一次 DNS 抖动就误伤正常链接。
            let resolved = tokio::task::spawn_blocking(move || {
                (name.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|addr| addr.ip()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            resolved.iter().any(|ip| ip_is_internal(*ip))
        }
    }
}

fn ip_is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_internal(v4),
        IpAddr::V6(v6) => v6_is_internal(v6),
    }
}

fn v4_is_internal(ip: Ipv4Addr) -> bool {
    let first = ip.octets()[0];
    ip.is_loopback()          // 127.0.0.0/8
        || ip.is_private()    // 10/8、172.16/12、192.168/16
        || ip.is_link_local() // 169.254.0.0/16，含云元数据 169.254.169.254
        || ip.is_unspecified()
        || ip.is_multicast()
        || first == 0         // 0.0.0.0/8
        || first >= 240       // 240.0.0.0/4 保留段
}

fn v6_is_internal(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_is_internal(v4);
    }
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 唯一本地
        || (ip.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 链路本地
}

/// 缩放因子落在合理区间，非有限值回落到默认的 1.0。
fn scale_factor(scale: f64) -> f64 {
    if scale.is_finite() {
        scale.clamp(0.5, 4.0)
    } else {
        1.0
    }
}

// ================= Core Logic =================

/// 截一张图。闸门、总超时和页面清理都在这里，`capture_page` 只管渲染。
async fn capture_url(url: &str, config: &Config, browser_path: Option<String>) -> Result<String> {
    let _permit = CAPTURE_GATE
        .acquire()
        .await
        .map_err(|_| anyhow!("截图闸门不可用"))?;

    let budget = Duration::from_secs(config.timeout_seconds.clamp(5, 120) + 15);
    // page 放在超时之外：无论正常返回、报错还是超时，都能走到下面的清理。
    let mut page = None;
    let result = time::timeout(budget, async {
        let browser = match browser_path.filter(|p| !p.is_empty()) {
            Some(path) => Browser::instance_with_path(path).await,
            None => Browser::instance().await,
        };
        page = Some(browser.new_tab().await?);
        capture_page(page.as_ref().unwrap(), url, config).await
    })
    .await;

    if let Some(tab) = page {
        let _ = time::timeout(Duration::from_secs(3), tab.close()).await;
    }

    match result {
        Ok(result) => result,
        Err(_) => Err(anyhow!("截图总耗时超时")),
    }
}

async fn capture_page(tab: &cdp_html_shot::Tab, url: &str, config: &Config) -> Result<String> {
    let width = config.viewport_width.clamp(200, 4096);
    let scale = scale_factor(config.device_scale_factor);
    let load_timeout = Duration::from_secs(config.timeout_seconds.clamp(5, 120));

    tab.set_viewport(&Viewport::new(width, 800).with_device_scale_factor(scale))
        .await?;

    match time::timeout(load_timeout, tab.goto(url)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(anyhow!("Navigate failed: {}", e)),
        Err(_) => return Err(anyhow!("Page load timeout")),
    };

    // 等待页面渲染
    time::sleep(Duration::from_millis(1000)).await;

    // 计算页面高度
    let height_js = "Math.max(document.body.scrollHeight, document.documentElement.scrollHeight)";
    let page_height = tab.evaluate(height_js).await?.as_f64().unwrap_or(800.0) as u32;

    // 先按配置上限收口，再按像素预算收口，超长页面不会把内存吃干。
    let max_height = config.max_height.clamp(100, 20000);
    let pixel_cap = (MAX_CAPTURE_PIXELS / (f64::from(width) * scale * scale)).floor().max(100.0);
    let final_height = (page_height.max(100).min(max_height) as f64).min(pixel_cap) as u32;

    let capture_viewport = Viewport::new(width, final_height).with_device_scale_factor(scale);

    tab.set_viewport(&capture_viewport).await?;

    if page_height > 800 {
        time::sleep(Duration::from_millis(500)).await;
    }

    let quality = config.quality.clamp(1, 100);
    let format = if quality >= 100 {
        ImageFormat::Png
    } else {
        ImageFormat::Jpeg
    };

    let opts = CaptureOptions::new()
        .with_viewport(capture_viewport)
        .with_format(format)
        .with_quality(quality)
        .with_full_page(true);

    tab.screenshot(opts)
        .await
        .map_err(|e| anyhow!("Screenshot failed: {}", e))
}

// ================= Main Handler =================

pub fn handle(
    ctx: Context,
    writer: LockedWriter,
) -> BoxFuture<'static, Result<Option<Context>, PluginError>> {
    Box::pin(async move {
        // 尝试解析为消息事件
        let msg_event = match ctx.as_message() {
            Some(e) => e,
            None => return Ok(Some(ctx)),
        };

        // 读取配置
        let config: Config = get_config_or_default(&ctx, "webshot");

        // 获取全局浏览器路径配置
        let browser_path = ctx.config.read().unwrap().browser_path.clone();

        // 检查群组黑白名单
        let group_id = msg_event.group_id();
        if !config.channel.allows(group_id) {
            return Ok(Some(ctx));
        }

        let user_id = msg_event.user_id();
        let self_id = ctx.bot.login_user.id.parse::<i64>().unwrap_or(0);

        if user_id == self_id {
            return Ok(Some(ctx));
        }

        // 提取 URL
        let url_candidate = if let crate::event::EventType::Satori(event) = &ctx.event {
            if let Some(arr) = event.get_array("message") {
                arr.iter()
                    .filter(|seg| seg.get_str("type") == Some("text"))
                    .find_map(|seg| {
                        seg.get("data")
                            .and_then(|d| d.get_str("text"))
                            .and_then(find_url)
                    })
            } else {
                find_url(msg_event.text())
            }
        } else {
            find_url(msg_event.text())
        };

        if let Some(candidate) = url_candidate {
            let url = match check_url(&candidate, &config).await {
                Ok(url) => url,
                Err(reason) => {
                    info!(target: "Plugin/WebShot", "跳过截图：{}", reason);
                    return Ok(Some(ctx));
                }
            };

            // 执行截图
            info!(target: "Plugin/WebShot", "Capturing: {}", url);

            match capture_url(url.as_str(), &config, browser_path).await {
                Ok(base64_img) => {
                    let msg = Message::new()
                        .reply(msg_event.message_id())
                        .image(format!("base64://{}", base64_img));

                    send_msg(&ctx, writer, group_id, Some(user_id), msg).await?;
                }
                Err(e) => {
                    error!(target: "Plugin/WebShot", "Error capturing {}: {}", url, e);
                }
            }
        }

        Ok(Some(ctx))
    })
}

/// Validate control edits against the plugin's actual configuration type.
pub fn validate_config(value: &toml::Value) -> Result<(), String> {
    <Config as serde::Deserialize>::deserialize(value.clone())
        .map(|_| ())
        .map_err(|_| "配置类型不匹配（请检查数组元素、字段类型及整数范围）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn domain_rules_match_on_suffix_not_substring() {
        assert!(domain_matches("evil.com", "evil.com"));
        assert!(domain_matches("a.evil.com", "evil.com"));
        assert!(domain_matches("a.evil.com", ".evil.com"));
        assert!(domain_matches("evil.com", " Evil.COM "));
        // 旧实现用 `url.contains`，这两种都会被误当成命中。
        assert!(!domain_matches("evil.com.attacker.net", "evil.com"));
        assert!(!domain_matches("notevil.com", "evil.com"));
        assert!(!domain_matches("example.com", ""));
    }

    #[test]
    fn internal_ip_ranges_are_recognised() {
        for blocked in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.5.5",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(ip_is_internal(ip), "{blocked} 应判为内网");
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "2001:4860:4860::8888"] {
            let ip: IpAddr = allowed.parse().unwrap();
            assert!(!ip_is_internal(ip), "{allowed} 不应判为内网");
        }
    }

    #[tokio::test]
    async fn private_and_non_http_links_are_rejected() {
        let config = config();
        for raw in [
            "http://127.0.0.1:6520/panel",
            "http://localhost:3001/v1/proxy/https://x",
            "http://[::1]/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://127.0.0.1./",
            "file:///etc/passwd",
        ] {
            assert!(check_url(raw, &config).await.is_err(), "{raw} 不应放行");
        }
        assert!(check_url("https://example.com/a?b=1", &config).await.is_ok());
    }

    #[tokio::test]
    async fn ignore_list_and_opt_in_are_respected() {
        let mut config = config();
        config.ignore_domains = vec!["evil.com".into()];
        assert!(check_url("https://a.evil.com/x", &config).await.is_err());

        // 显式放开后，内网字面量可以截图；忽略名单仍然生效。
        config.allow_private_hosts = true;
        assert!(check_url("http://192.168.1.1/", &config).await.is_ok());
        assert!(check_url("http://evil.com/", &config).await.is_err());
    }

    #[test]
    fn scale_factor_is_bounded_even_for_non_finite_values() {
        assert_eq!(scale_factor(f64::NAN), 1.0);
        assert_eq!(scale_factor(f64::INFINITY), 1.0);
        assert_eq!(scale_factor(0.1), 0.5);
        assert_eq!(scale_factor(9.0), 4.0);
    }
}
