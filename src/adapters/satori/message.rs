use crate::message::{Message, Segment};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;
use simd_json::OwnedValue;
use simd_json::base::{ValueAsArray, ValueAsObject, ValueAsScalar};
use simd_json::derived::{ValueObjectAccess, ValueObjectAccessAsScalar};
use simd_json::owned::Object;
use std::sync::{Arc, OnceLock};

#[derive(Default)]
struct Element {
    name: String,
    attrs: Object,
    children: Vec<Element>,
    text: String,
}

/// 按 Satori 的[资源链接](https://satori.js.org/zh-CN/advanced/resource.html)规范，
/// 把元素里的 `src` 解析成插件可以直接 GET 的地址。
///
/// `internal:` 链接和 `proxy_urls` 命中的平台链接都要走实现端的 `/v1/proxy/{url}`
/// 路由——前者应用侧根本取不到，后者可能有防盗链或时效。该路由不需要鉴权头。
#[derive(Clone, Default)]
pub struct ResourceProxy {
    endpoint: String,
    proxy_urls: Arc<Vec<String>>,
}

impl ResourceProxy {
    pub fn new(endpoint: String, proxy_urls: Arc<Vec<String>>) -> Self {
        Self {
            endpoint,
            proxy_urls,
        }
    }

    /// 返回可直接下载的 URL；`data:`、`file:` 和本地路径没有下载地址，返回 `None`。
    fn fetchable(&self, src: &str) -> Option<String> {
        let direct = src.starts_with("http://") || src.starts_with("https://");
        let proxied = src.starts_with("internal:")
            || (direct
                && self
                    .proxy_urls
                    .iter()
                    .any(|prefix| src.starts_with(prefix.as_str())));
        if proxied && !self.endpoint.is_empty() {
            return Some(format!(
                "{}/v1/proxy/{}",
                self.endpoint,
                encode_proxy_url(src)
            ));
        }
        direct.then(|| src.to_string())
    }
}

/// 代理路由把 URL 直接拼在路径里，只转义会破坏路径解析的字符。
/// `internal:` 链接不含这些字符，因此始终保持原样。
fn encode_proxy_url(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for ch in src.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '+' => out.push_str("%2B"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    out
}

/// 将 Satori 元素串转为框架内部的消息段，按实现端下发的代理路由解析资源地址。
pub fn from_content_with(content: &str, proxy: &ResourceProxy) -> Message {
    let normalized = normalize_boolean_attrs(content);
    let mut reader = Reader::from_str(&normalized);
    reader.config_mut().trim_text(false);
    let mut roots = Vec::new();
    let mut stack: Vec<Element> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(start)) => {
                stack.push(Element {
                    name: String::from_utf8_lossy(start.name().as_ref()).into_owned(),
                    attrs: attrs(&reader, &start),
                    ..Default::default()
                });
            }
            Ok(XmlEvent::Empty(start)) => {
                let element = Element {
                    name: String::from_utf8_lossy(start.name().as_ref()).into_owned(),
                    attrs: attrs(&reader, &start),
                    ..Default::default()
                };
                push_element(&mut roots, &mut stack, element);
            }
            Ok(XmlEvent::Text(text)) => {
                let decoded = text.decode().unwrap_or_default();
                let value = match quick_xml::escape::unescape(&decoded) {
                    Ok(value) => value.into_owned(),
                    Err(_) => decoded.into_owned(),
                };
                push_text(&mut roots, &mut stack, &value);
            }
            Ok(XmlEvent::CData(text)) => {
                let value = text.decode().unwrap_or_default();
                push_text(&mut roots, &mut stack, &value);
            }
            Ok(XmlEvent::GeneralRef(reference)) => {
                let name = reference.decode().unwrap_or_default();
                let value = if let Some(number) = name.strip_prefix("#x") {
                    u32::from_str_radix(number, 16)
                        .ok()
                        .and_then(char::from_u32)
                } else if let Some(number) = name.strip_prefix('#') {
                    number.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
                .map(|value| value.to_string())
                .or_else(|| quick_xml::escape::resolve_xml_entity(&name).map(str::to_string))
                .unwrap_or_else(|| format!("&{name};"));
                push_text(&mut roots, &mut stack, &value);
            }
            Ok(XmlEvent::End(_)) => {
                if let Some(element) = stack.pop() {
                    push_element(&mut roots, &mut stack, element);
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(_) => return Message::new().text(content),
            _ => {}
        }
    }

    let mut out = Message::new();
    for root in roots {
        append_element(&mut out, root, proxy);
    }
    out
}

/// Satori 允许 `<message forward>` 这样的布尔属性；补成严格 XML 交给 quick-xml。
fn normalize_boolean_attrs(content: &str) -> String {
    static TAG: OnceLock<regex::Regex> = OnceLock::new();
    static ATTR: OnceLock<regex::Regex> = OnceLock::new();
    let tag = TAG.get_or_init(|| {
        regex::Regex::new(r#"<([a-z][a-z0-9-]*)([^<>]*?)(/?)>"#).expect("valid tag regex")
    });
    let attr = ATTR.get_or_init(|| {
        regex::Regex::new(r#"([^\s=]+)(?:=(?:"[^"]*"|'[^']*'))?"#).expect("valid attr regex")
    });
    tag.replace_all(content, |caps: &regex::Captures<'_>| {
        let name = caps.get(1).map(|value| value.as_str()).unwrap_or("");
        let raw_attrs = caps.get(2).map(|value| value.as_str()).unwrap_or("");
        let slash = caps.get(3).map(|value| value.as_str()).unwrap_or("");
        let mut attrs = String::new();
        for found in attr.find_iter(raw_attrs) {
            let raw = found.as_str();
            if raw.contains('=') {
                attrs.push(' ');
                attrs.push_str(raw);
            } else if let Some(key) = raw.strip_prefix("no-") {
                attrs.push_str(&format!(" {key}=\"false\""));
            } else {
                attrs.push_str(&format!(" {raw}=\"true\""));
            }
        }
        format!("<{name}{attrs}{slash}>")
    })
    .into_owned()
}

fn push_text(roots: &mut Vec<Element>, stack: &mut [Element], value: &str) {
    if value.is_empty() {
        return;
    }
    let target = if let Some(parent) = stack.last_mut() {
        &mut parent.children
    } else {
        roots
    };
    if let Some(last) = target.last_mut()
        && last.name == "text"
    {
        last.text.push_str(value);
        return;
    }
    target.push(Element {
        name: "text".to_string(),
        text: value.to_string(),
        ..Default::default()
    });
}

fn attrs(reader: &Reader<&[u8]>, start: &quick_xml::events::BytesStart<'_>) -> Object {
    let mut out = Object::new();
    for attr in start.attributes().with_checks(false).flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let value = attr
            .decode_and_unescape_value(reader.decoder())
            .map(|v| v.into_owned())
            .unwrap_or_default();
        out.insert(key, OwnedValue::from(value));
    }
    out
}

fn push_element(roots: &mut Vec<Element>, stack: &mut [Element], element: Element) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(element);
    } else {
        roots.push(element);
    }
}

fn attr(element: &Element, key: &str) -> String {
    element
        .attrs
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn append_element(out: &mut Message, element: Element, proxy: &ResourceProxy) {
    let mut data = Object::new();
    match element.name.as_str() {
        "text" => push(out, "text", "text", element.text),
        "at" => {
            let id = attr(&element, "id");
            let at_type = attr(&element, "type");
            let role = attr(&element, "role");
            let target = if id.eq_ignore_ascii_case("all")
                || at_type.eq_ignore_ascii_case("all")
                || (id.is_empty() && role.eq_ignore_ascii_case("all"))
            {
                "all".to_string()
            } else {
                id
            };
            if target.is_empty() {
                let fallback = if at_type.eq_ignore_ascii_case("here") {
                    "在线成员".to_string()
                } else {
                    [attr(&element, "name"), role, at_type]
                        .into_iter()
                        .find(|value| !value.is_empty())
                        .unwrap_or_default()
                };
                if !fallback.is_empty() {
                    push(out, "text", "text", format!("@{fallback}"));
                }
                return;
            }
            data.insert("qq".into(), OwnedValue::from(target));
            let name = attr(&element, "name");
            if !name.is_empty() {
                data.insert("name".into(), OwnedValue::from(name));
            }
            out.0.push(Segment::new("at", data));
        }
        "sharp" => {
            let label = [attr(&element, "name"), attr(&element, "id")]
                .into_iter()
                .find(|value| !value.is_empty())
                .unwrap_or_default();
            if !label.is_empty() {
                push(out, "text", "text", format!("#{label}"));
            }
        }
        "quote" => {
            let id = [
                attr(&element, "id"),
                attr(&element, "message-id"),
                attr(&element, "messageId"),
            ]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or_default();
            push(out, "reply", "id", id);
        }
        "face" | "emoji" => push(out, "face", "id", attr(&element, "id")),
        "img" | "image" => resource(out, "image", element, proxy),
        "audio" | "record" => resource(out, "record", element, proxy),
        "video" => resource(out, "video", element, proxy),
        "file" => resource(out, "file", element, proxy),
        "json" => {
            let value = if attr(&element, "data").is_empty() {
                joined_text(&element)
            } else {
                attr(&element, "data")
            };
            push(out, "json", "data", value);
        }
        "mface" | "poke" => {
            let data = element
                .attrs
                .into_iter()
                .map(|(key, value)| {
                    let key = key.replace('-', "_");
                    let value = if key == "sub_type" {
                        value
                            .as_str()
                            .and_then(|value| value.parse::<i64>().ok())
                            .map(OwnedValue::from)
                            .unwrap_or(value)
                    } else {
                        value
                    };
                    (key, value)
                })
                .collect();
            out.0.push(Segment::new(&element.name, data));
        }
        "br" => push(out, "text", "text", "\n".to_string()),
        "p" => {
            append_children(out, element.children, proxy);
            push(out, "text", "text", "\n".to_string());
        }
        "a" => {
            let href = attr(&element, "href");
            let label = joined_text(&element);
            append_children(out, element.children, proxy);
            if !href.is_empty() && href != label {
                let suffix = if label.is_empty() {
                    href
                } else {
                    format!(" ({href})")
                };
                push(out, "text", "text", suffix);
            }
        }
        "message" if attr(&element, "forward") == "true" => {
            let id = attr(&element, "id");
            if !id.is_empty() {
                push(out, "forward", "id", id);
            } else {
                for child in element.children {
                    if child.name != "message" {
                        continue;
                    }
                    let mut node = Object::new();
                    let mut body = Message::new();
                    for part in child.children {
                        if part.name == "author" {
                            node.insert("user_id".into(), OwnedValue::from(attr(&part, "id")));
                            node.insert("nickname".into(), OwnedValue::from(attr(&part, "name")));
                        } else {
                            append_element(&mut body, part, proxy);
                        }
                    }
                    node.insert(
                        "content".into(),
                        simd_json::serde::to_owned_value(body).unwrap_or_default(),
                    );
                    out.0.push(Segment::new("node", node));
                }
            }
        }
        _ => {
            if !element.text.is_empty() {
                push(out, "text", "text", element.text);
            }
            append_children(out, element.children, proxy);
        }
    }
}

fn joined_text(element: &Element) -> String {
    let mut text = element.text.clone();
    for child in &element.children {
        text.push_str(&joined_text(child));
    }
    text
}

fn append_children(out: &mut Message, children: Vec<Element>, proxy: &ResourceProxy) {
    for child in children {
        append_element(out, child, proxy);
    }
}

fn push(out: &mut Message, kind: &str, key: &str, value: String) {
    let mut data = Object::new();
    data.insert(key.into(), OwnedValue::from(value));
    out.0.push(Segment::new(kind, data));
}

fn resource(out: &mut Message, kind: &str, element: Element, proxy: &ResourceProxy) {
    let mut data = Object::new();
    let src = attr(&element, "src");
    if let Some(url) = proxy.fetchable(&src) {
        data.insert("url".into(), OwnedValue::from(url));
    }
    data.insert("file".into(), OwnedValue::from(src));
    let title = attr(&element, "title");
    if !title.is_empty() {
        data.insert("name".into(), OwnedValue::from(title));
    }
    for (source, target) in [
        ("file-size", "file_size"),
        ("fileSize", "file_size"),
        ("width", "width"),
        ("height", "height"),
        ("duration", "duration"),
        ("poster", "poster"),
    ] {
        let value = attr(&element, source);
        if !value.is_empty() {
            data.insert(target.into(), OwnedValue::from(value));
        }
    }
    out.0.push(Segment::new(kind, data));
}

/// 将框架内部消息段序列化为 Satori 元素串。
pub fn to_content(value: &OwnedValue) -> String {
    if let Some(text) = value.as_str() {
        return escape_text(text);
    }
    let Some(segments) = value.as_array() else {
        return String::new();
    };
    let all_nodes = !segments.is_empty()
        && segments
            .iter()
            .all(|segment| segment.get_str("type") == Some("node"));
    let body = segments.iter().map(segment_to_content).collect::<String>();
    if all_nodes {
        format!("<message forward>{body}</message>")
    } else {
        body
    }
}

fn segment_to_content(segment: &OwnedValue) -> String {
    let kind = segment.get_str("type").unwrap_or("text");
    let data = segment.get("data").unwrap_or(segment);
    match kind {
        "text" => escape_text(data.get_str("text").unwrap_or("")),
        "at" => {
            let id = scalar(data.get("qq"));
            if id.eq_ignore_ascii_case("all") {
                "<at type=\"all\"/>".to_string()
            } else {
                format!("<at id=\"{}\"/>", escape_attr(&id))
            }
        }
        "face" => tag("emoji", &[("id", scalar(data.get("id")))]),
        "reply" => tag("quote", &[("id", scalar(data.get("id")))]),
        "image" => resource_tag("img", data),
        "record" => resource_tag("audio", data),
        "video" => resource_tag("video", data),
        "file" => resource_tag("file", data),
        "json" | "lightapp" => tag(
            "json",
            &[(
                "data",
                data.get_str("data")
                    .or_else(|| data.get_str("content"))
                    .unwrap_or("")
                    .to_string(),
            )],
        ),
        "mface" | "poke" => tag_from_data(kind, data),
        "dice" | "rps" => format!("<{kind}/>"),
        "markdown" => escape_text(data.get_str("content").unwrap_or("")),
        "node" => {
            if let Some(id) = data.get_str("id") {
                return format!("<message id=\"{}\"/>", escape_attr(id));
            }
            let uid = scalar(data.get("user_id"));
            let nick = data.get_str("nickname").unwrap_or("");
            let content = data.get("content").map(to_content).unwrap_or_default();
            format!(
                "<message><author id=\"{}\" name=\"{}\"/>{}</message>",
                escape_attr(&uid),
                escape_attr(nick),
                content
            )
        }
        "forward" => tag(
            "message",
            &[
                ("forward", "true".to_string()),
                ("id", scalar(data.get("id"))),
            ],
        ),
        _ => escape_text(data.get_str("text").unwrap_or("")),
    }
}

fn resource_tag(kind: &str, data: &OwnedValue) -> String {
    let src = data
        .get_str("file")
        .or_else(|| data.get_str("url"))
        .unwrap_or("");
    let mut attrs = vec![("src", src.to_string())];
    if let Some(name) = data.get_str("name") {
        attrs.push(("title", name.to_string()));
    }
    tag(kind, &attrs)
}

fn tag_from_data(kind: &str, data: &OwnedValue) -> String {
    let Some(object) = data.as_object() else {
        return format!("<{kind}/>");
    };
    let attrs = object
        .iter()
        .map(|(key, value)| (key.as_str(), scalar(Some(value))))
        .collect::<Vec<_>>();
    tag(kind, &attrs)
}

fn tag(kind: &str, attrs: &[(&str, String)]) -> String {
    let attrs = attrs
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| format!(" {key}=\"{}\"", escape_attr(value)))
        .collect::<String>();
    format!("<{kind}{attrs}/>")
}

fn scalar(value: Option<&OwnedValue>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(value) = value.as_str() {
        value.to_string()
    } else if let Some(value) = value.as_i64() {
        value.to_string()
    } else if let Some(value) = value.as_u64() {
        value.to_string()
    } else if let Some(value) = value.as_bool() {
        value.to_string()
    } else {
        String::new()
    }
}

/// 只转义 `< > &`：实现端的反转义不认 `&apos;`，全量转义会把
/// `don&apos;t` 这种半成品原样发进 QQ 消息。
fn escape_plain(value: &str) -> String {
    quick_xml::escape::partial_escape(value).into_owned()
}

/// Satori 规范把文本段首尾「含换行的空白」当排版空白裁掉，首尾换行因此改用
/// `<br/>` 表达，否则 `@某人\n[图片]` 之类的换行会被实现端吃掉。
fn escape_text(value: &str) -> String {
    let Some(start) = value.find(|ch: char| !ch.is_whitespace()) else {
        return boundary_whitespace(value);
    };
    let end = value
        .rfind(|ch: char| !ch.is_whitespace())
        .map(|idx| idx + value[idx..].chars().next().map_or(0, char::len_utf8))
        .unwrap_or(value.len());
    format!(
        "{}{}{}",
        boundary_whitespace(&value[..start]),
        escape_plain(&value[start..end]),
        boundary_whitespace(&value[end..])
    )
}

/// 首尾空白里的换行转成 `<br/>`；`\r` 会让实现端把整段空白判成排版空白，直接丢掉。
fn boundary_whitespace(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("<br/>"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(value: &str) -> String {
    escape_plain(value).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不涉及代理路由的用例走默认解析。
    fn from_content(content: &str) -> Message {
        from_content_with(content, &ResourceProxy::default())
    }

    #[test]
    fn parses_satori_elements() {
        let message = from_content(
            "hi &amp; <at id=\"42\"/> <quote id=\"7000000000000000000\"/><img src=\"https://x/y?a=1&amp;b=2\"/>",
        );
        assert_eq!(message.0.len(), 5);
        assert_eq!(
            message.0[0].data.get("text").and_then(|v| v.as_str()),
            Some("hi & ")
        );
        assert_eq!(
            message.0[1].data.get("qq").and_then(|v| v.as_str()),
            Some("42")
        );
        assert_eq!(
            message.0[2].data.get("text").and_then(|v| v.as_str()),
            Some(" ")
        );
        assert_eq!(
            message.0[3].data.get("id").and_then(|v| v.as_str()),
            Some("7000000000000000000")
        );
        assert_eq!(
            message.0[4].data.get("file").and_then(|v| v.as_str()),
            Some("https://x/y?a=1&b=2")
        );
    }

    #[test]
    fn serializes_message_chain() {
        let message = Message::new()
            .text("a < b")
            .at(42)
            .reply("7000000000000000000")
            .image("https://x/y?a=1&b=2");
        let value = simd_json::serde::to_owned_value(message).unwrap();
        assert_eq!(
            to_content(&value),
            "a &lt; b<at id=\"42\"/><quote id=\"7000000000000000000\"/><img src=\"https://x/y?a=1&amp;b=2\"/>"
        );
    }

    #[test]
    fn parses_satori_boolean_attributes_and_forward_nodes() {
        let message = from_content(
            "<message forward><message><author id=\"42\" name=\"Alice\"/>hello</message></message>",
        );
        assert_eq!(message.0.len(), 1);
        assert_eq!(message.0[0].type_, "node");
        assert_eq!(
            message.0[0]
                .data
                .get("user_id")
                .and_then(|value| value.as_str()),
            Some("42")
        );
    }

    #[test]
    fn normalizes_plugin_visible_attributes_and_fallback_text() {
        let message = from_content(
            "<mface emoji-id=\"1\" sub-type=\"1\" summary=\"[动画表情]\"/><at type=\"here\"/><sharp name=\"频道\"/><a href=\"https://example.com\">站点</a>",
        );
        assert_eq!(message.0[0].type_, "mface");
        assert_eq!(
            message.0[0]
                .data
                .get("sub_type")
                .and_then(|value| value.as_i64()),
            Some(1)
        );
        let text = message
            .0
            .iter()
            .filter(|segment| segment.type_ == "text")
            .filter_map(|segment| segment.data.get("text").and_then(|value| value.as_str()))
            .collect::<String>();
        assert_eq!(text, "@在线成员#频道站点 (https://example.com)");
    }

    /// 实现端的反转义不认 `&apos;`；撇号必须原样进元素串。
    #[test]
    fn keeps_apostrophes_out_of_entities() {
        let message = Message::new().text("don't <stop> \"now\"");
        let value = simd_json::serde::to_owned_value(message).unwrap();
        assert_eq!(to_content(&value), "don't &lt;stop&gt; \"now\"");
    }

    /// 属性里的双引号仍要转义，否则会截断元素串。
    #[test]
    fn escapes_quotes_only_inside_attributes() {
        let message = Message::new().image("https://x/y?q=\"a\"&z='b'");
        let value = simd_json::serde::to_owned_value(message).unwrap();
        assert_eq!(
            to_content(&value),
            "<img src=\"https://x/y?q=&quot;a&quot;&amp;z='b'\"/>"
        );
    }

    /// 首尾换行会被实现端当排版空白裁掉，必须以 `<br/>` 形式送出。
    #[test]
    fn boundary_newlines_survive_as_br() {
        let message = Message::new()
            .at(42)
            .text("\n")
            .text("提取成功：\n")
            .image("base64://AAAA");
        let value = simd_json::serde::to_owned_value(message).unwrap();
        assert_eq!(
            to_content(&value),
            "<at id=\"42\"/><br/>提取成功：<br/><img src=\"base64://AAAA\"/>"
        );
    }

    /// 段内换行不在首尾，实现端不会动它，保持原样以免拆成一堆文本段。
    #[test]
    fn inner_newlines_stay_literal() {
        let message = Message::new().text("第一行\n第二行");
        let value = simd_json::serde::to_owned_value(message).unwrap();
        assert_eq!(to_content(&value), "第一行\n第二行");
    }

    /// `internal:` 链接应用侧取不到，必须换成实现端的代理路由。
    #[test]
    fn internal_links_resolve_through_the_proxy_route() {
        let proxy = ResourceProxy::new("http://127.0.0.1:3001".to_string(), Arc::new(Vec::new()));
        let message = from_content_with(
            "<img src=\"internal:red/10000/_tmp/satori-res:image:1\"/>",
            &proxy,
        );
        assert_eq!(
            message.0[0].data.get("url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:3001/v1/proxy/internal:red/10000/_tmp/satori-res:image:1")
        );
        // file 仍保留实现端给的原始 src，便于回传。
        assert_eq!(
            message.0[0].data.get("file").and_then(|v| v.as_str()),
            Some("internal:red/10000/_tmp/satori-res:image:1")
        );
    }

    /// 命中 proxy_urls 的平台链接有防盗链/时效，同样走代理路由；其余直连。
    #[test]
    fn only_declared_prefixes_are_proxied() {
        let proxy = ResourceProxy::new(
            "http://127.0.0.1:3001".to_string(),
            Arc::new(vec!["https://gchat.qpic.cn/".to_string()]),
        );
        let hotlinked = from_content_with("<img src=\"https://gchat.qpic.cn/a?b=1\"/>", &proxy);
        assert_eq!(
            hotlinked.0[0].data.get("url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:3001/v1/proxy/https://gchat.qpic.cn/a%3Fb=1")
        );
        let plain = from_content_with("<img src=\"https://example.com/a.png\"/>", &proxy);
        assert_eq!(
            plain.0[0].data.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/a.png")
        );
    }

    /// `data:` 与本地路径没有可下载地址，插件不该拿到假的 url。
    #[test]
    fn undownloadable_sources_have_no_url() {
        let message = from_content("<img src=\"data:image/png;base64,AAAA\"/>");
        assert!(message.0[0].data.get("url").is_none());
        assert_eq!(
            message.0[0].data.get("file").and_then(|v| v.as_str()),
            Some("data:image/png;base64,AAAA")
        );
    }
}
