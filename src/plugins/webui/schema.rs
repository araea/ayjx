//! 从插件的默认配置推导出一张表单。
//!
//! 默认配置就是这套框架里最接近「配置模式」的东西：`ctl` 拿它做形状校验，
//! 面板拿它决定每一项该画成什么控件。两处读的是同一份数据，于是表单里出现的
//! 每一项都一定改得动，改不动的项也一定不会出现。
//!
//! 一个例外要处理：**默认是空数组时元素类型无从得知**（`groups = []`）。
//! 与其猜，不如问——把一个只含试探元素的候选配置交给插件自己的
//! `validate_config` 走一遍，谁不报错就是谁。这条路子的好处是答案永远和运行时
//! 的真实类型一致，插件改了字段类型也不会漏。

use crate::plugins::Plugin;
use crate::plugins::ctl;
use serde::Serialize;
use serde_json::{Value as Json, json};
use toml::Value;

/// 表单里的一项。`table` 用 `children` 承载下一层，其余种类都是叶子。
#[derive(Serialize)]
pub(super) struct Field {
    /// 点分路径，与 `/ctl set <插件> <路径>` 完全一致
    pub path: String,
    /// 本层键名
    pub key: String,
    /// 控件种类：bool / int / float / string / bool_array / int_array /
    /// float_array / string_array / table / raw
    pub kind: &'static str,
    /// 当前值（敏感项为 null）
    pub value: Json,
    /// 默认值（敏感项为 null）
    pub default: Json,
    /// 与默认值不同
    pub changed: bool,
    /// 敏感项：值不出网，只说明填没填
    pub sensitive: bool,
    /// 敏感项当前是否已填
    pub filled: bool,
    /// 取值范围或单位的一句话提示，没有就是空串
    pub hint: &'static str,
    /// 只能取固定几个值时的候选清单（来自 `ctl::options`），否则为空
    pub options: &'static [&'static str],
    pub children: Vec<Field>,
}

/// 把一份插件配置展开成表单。`enabled` 由页面顶部的总开关承担，不进表单。
pub(super) fn fields(plugin: &'static Plugin, current: &Value, defaults: &Value) -> Vec<Field> {
    let (Value::Table(current_table), Value::Table(default_table)) = (current, defaults) else {
        return Vec::new();
    };
    // 顺序以默认配置为准（它是稳定的、有意义的书写顺序），
    // 磁盘上多出来的键补在后面，不让它们消失。
    let mut keys: Vec<&String> = default_table.keys().filter(|k| *k != "enabled").collect();
    keys.extend(
        current_table
            .keys()
            .filter(|k| *k != "enabled" && !default_table.contains_key(*k)),
    );
    keys.into_iter()
        .map(|key| {
            field(
                plugin,
                current,
                key,
                key,
                current_table.get(key),
                default_table.get(key),
            )
        })
        .collect()
}

fn field(
    plugin: &'static Plugin,
    root: &Value,
    path: &str,
    key: &str,
    current: Option<&Value>,
    default: Option<&Value>,
) -> Field {
    // 形状看默认值，没有默认值（磁盘上的额外项）才退回当前值。
    let shape = default.or(current);
    let sensitive = ctl::sensitive(path);
    let kind = kind_of(plugin, root, path, shape, current);

    let mut children = Vec::new();
    if kind == "table" {
        let empty = toml::map::Map::new();
        let current_table = current.and_then(Value::as_table).unwrap_or(&empty);
        let default_table = default.and_then(Value::as_table).unwrap_or(&empty);
        let mut keys: Vec<&String> = default_table.keys().collect();
        keys.extend(
            current_table
                .keys()
                .filter(|k| !default_table.contains_key(*k)),
        );
        children = keys
            .into_iter()
            .map(|child| {
                field(
                    plugin,
                    root,
                    &format!("{path}.{child}"),
                    child,
                    current_table.get(child),
                    default_table.get(child),
                )
            })
            .collect();
    }

    let filled = current
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    Field {
        path: path.to_string(),
        key: key.to_string(),
        kind,
        value: if sensitive {
            Json::Null
        } else {
            current.map(to_json).unwrap_or(Json::Null)
        },
        default: if sensitive {
            Json::Null
        } else {
            default.map(to_json).unwrap_or(Json::Null)
        },
        changed: match (current, default) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        },
        sensitive,
        filled,
        hint: hint(path, shape),
        options: ctl::options(plugin.name, path),
        children,
    }
}

fn kind_of(
    plugin: &'static Plugin,
    root: &Value,
    path: &str,
    shape: Option<&Value>,
    current: Option<&Value>,
) -> &'static str {
    match shape {
        Some(Value::Boolean(_)) => "bool",
        Some(Value::Integer(_)) => "int",
        Some(Value::Float(_)) => "float",
        Some(Value::String(_)) | Some(Value::Datetime(_)) => "string",
        // 默认是空表意味着「键由用户自己定」（按群保存的偏好就是这样），
        // 画不成固定表单，交给 TOML 文本框；有默认键的表才展开成分组。
        Some(Value::Table(t)) => {
            if t.is_empty() {
                "raw"
            } else {
                "table"
            }
        }
        Some(Value::Array(a)) => match a.first().or_else(|| {
            current
                .and_then(Value::as_array)
                .and_then(|values| values.first())
        }) {
            Some(Value::Boolean(_)) => "bool_array",
            Some(Value::Integer(_)) => "int_array",
            Some(Value::Float(_)) => "float_array",
            Some(Value::String(_)) => "string_array",
            Some(_) => "raw",
            // 两头都空：问插件自己的类型
            None => probe_array(plugin, root, path),
        },
        None => "raw",
    }
}

/// 用一个试探元素问出空数组的元素类型。插件的 `validate_config` 走的是真实的
/// serde 类型，答案不会和运行时对不上。
fn probe_array(plugin: &'static Plugin, root: &Value, path: &str) -> &'static str {
    for (probe, kind) in [
        (Value::Integer(1), "int_array"),
        (Value::String("probe".into()), "string_array"),
        (Value::Float(1.0), "float_array"),
        (Value::Boolean(true), "bool_array"),
    ] {
        let mut candidate = root.clone();
        if let Some(slot) = at_mut(&mut candidate, path) {
            *slot = Value::Array(vec![probe]);
            if ctl::validate(plugin, &candidate).is_ok() {
                return kind;
            }
        }
    }
    "raw"
}

fn at_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('.').try_fold(value, |v, key| match v {
        Value::Array(a) => a.get_mut(key.parse::<usize>().ok()?),
        Value::Table(t) => t.get_mut(key),
        _ => None,
    })
}

/// 看着像一个时刻（`08:20` / `08:20:00`）
fn looks_like_time(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| (1..=2).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
}

/// 取值范围与单位。只写框架里能确定的那几类，不替插件编造语义。
///
/// 键名单独看会骗人——`min_times` 是「次数」不是「时刻」——所以凡是靠名字推断
/// 语义的规则都拿默认值再验一遍，形状对不上就不说话。
fn hint(path: &str, shape: Option<&Value>) -> &'static str {
    let key = path.rsplit('.').next().unwrap_or(path);
    if key.contains("probability") {
        return "0 – 1";
    }
    if key == "image_scale" {
        return "1 – 4";
    }
    if (key == "time" || key.ends_with("_time"))
        && shape.and_then(Value::as_str).is_some_and(looks_like_time)
    {
        return "HH:MM 或 HH:MM:SS";
    }
    if let Some(Value::Array(items)) = shape
        && !items.is_empty()
        && items
            .iter()
            .all(|item| item.as_str().is_some_and(looks_like_time))
    {
        return "每项 HH:MM:SS";
    }
    if key.ends_with("_seconds") || key == "seconds" {
        return "秒";
    }
    if key.ends_with("_ms") {
        return "毫秒";
    }
    if key.ends_with("_minutes") {
        return "分钟";
    }
    if key.ends_with("_hours") {
        return "小时";
    }
    if key.ends_with("_days") {
        return "天";
    }
    if key.ends_with("_mb") {
        return "MB";
    }
    if key.ends_with("_chars") {
        return "字符数";
    }
    if key == "white" || key == "black" || key == "groups" || key.ends_with("_groups") {
        return "群号";
    }
    if key == "admins" || key.ends_with("_users") {
        return "QQ 号";
    }
    ""
}

// ================= TOML ↔ JSON =================

pub(super) fn to_json(value: &Value) -> Json {
    match value {
        Value::String(s) => json!(s),
        Value::Integer(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Boolean(b) => json!(b),
        Value::Datetime(d) => json!(d.to_string()),
        Value::Array(a) => Json::Array(a.iter().map(to_json).collect()),
        Value::Table(t) => Json::Object(t.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

/// JSON → TOML，按现有值的类型收敛。
///
/// 浏览器只有一种数字类型，`2` 送到一个 `f64` 字段上必须落成 `Float`，否则
/// `ctl` 的形状校验会以「类型错误」拒绝一次本来正确的修改。
pub(super) fn to_toml(json: &Json, old: Option<&Value>) -> Result<Value, String> {
    Ok(match json {
        Json::Bool(b) => Value::Boolean(*b),
        Json::String(s) => Value::String(s.clone()),
        Json::Number(n) => {
            let float = matches!(old, Some(Value::Float(_)));
            if float {
                Value::Float(n.as_f64().ok_or("数字超出范围")?)
            } else if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else {
                Value::Float(n.as_f64().ok_or("数字超出范围")?)
            }
        }
        Json::Array(items) => {
            let element = old.and_then(Value::as_array).and_then(|a| a.first());
            Value::Array(
                items
                    .iter()
                    .map(|item| to_toml(item, element))
                    .collect::<Result<_, _>>()?,
            )
        }
        Json::Object(map) => {
            let table = old.and_then(Value::as_table);
            Value::Table(
                map.iter()
                    .map(|(k, v)| Ok((k.clone(), to_toml(v, table.and_then(|t| t.get(k)))?)))
                    .collect::<Result<toml::map::Map<_, _>, String>>()?,
            )
        }
        Json::Null => return Err("值不能为空".into()),
    })
}

/// TOML 文本 → 值。表按文档解析，其余按 `key = 值` 解析——与 `/ctl set` 同一套写法。
pub(super) fn parse_toml(text: &str, old: Option<&Value>) -> Result<Value, String> {
    if matches!(old, Some(Value::Table(_))) {
        return toml::from_str::<toml::Table>(text)
            .map(Value::Table)
            .map_err(|e| format!("TOML 解析失败：{e}"));
    }
    let wrapper: Value = toml::from_str(&format!("value = {}", text.trim()))
        .map_err(|_| "值格式错误；数组用 [1, 2]，表用 { key = \"值\" }".to_string())?;
    wrapper
        .get("value")
        .cloned()
        .ok_or_else(|| "缺少值".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::get_plugins;

    fn plugin(name: &str) -> &'static Plugin {
        get_plugins().iter().find(|p| p.name == name).unwrap()
    }

    /// 每个已注册插件的默认配置都必须能画成表单，且不留 raw 兜底以外的空洞。
    #[test]
    fn every_plugin_produces_a_form() {
        for p in get_plugins() {
            let defaults = (p.default_config)();
            let form = fields(p, &defaults, &defaults);
            for f in &form {
                assert!(!f.kind.is_empty(), "{}: {} 没有控件类型", p.name, f.path);
                assert!(!f.changed, "{}: {} 与自身默认值不同", p.name, f.path);
            }
        }
    }

    /// 空数组要问出真实元素类型，而不是猜成字符串。
    #[test]
    fn empty_arrays_are_probed_against_the_real_type() {
        let repeater = plugin("repeater");
        let defaults = (repeater.default_config)();
        let channel = fields(repeater, &defaults, &defaults)
            .into_iter()
            .find(|f| f.key == "channel")
            .expect("repeater 有 channel 表");
        let white = channel.children.iter().find(|f| f.key == "white").unwrap();
        assert_eq!(white.kind, "int_array", "群号列表应当推断为整数数组");

        let repeater_texts = fields(repeater, &defaults, &defaults)
            .into_iter()
            .find(|f| f.key == "interrupt_texts")
            .unwrap();
        assert_eq!(repeater_texts.kind, "string_array");
    }

    /// 固定取值的项要带上候选清单，面板才画得出下拉框。
    #[test]
    fn enumerated_fields_carry_their_choices() {
        let ai = plugin("ai_news");
        let defaults = (ai.default_config)();
        let form = fields(ai, &defaults, &defaults);
        let mode = form.iter().find(|f| f.key == "mode").unwrap();
        assert_eq!(mode.options, ["all", "selected"]);
        let limit = form.iter().find(|f| f.key == "limit").unwrap();
        assert!(limit.options.is_empty());
        // 面板给出的每个候选值都必须真的存得进去
        for choice in mode.options {
            let mut candidate = defaults.clone();
            candidate["mode"] = Value::String((*choice).into());
            ctl::validate(ai, &candidate).unwrap_or_else(|e| panic!("{choice}: {e}"));
        }
    }

    #[test]
    fn secrets_never_leave_the_process() {
        let oai = plugin("oai");
        let mut current = (oai.default_config)();
        if let Some(table) = current.as_table_mut() {
            table.insert("api_key".into(), Value::String("real-secret".into()));
        }
        let form = fields(oai, &current, &(oai.default_config)());
        let json = serde_json::to_string(&form).unwrap();
        assert!(!json.contains("real-secret"), "{json}");
    }

    /// 名字像时刻、值却是次数的键不该被标成时间格式。
    #[test]
    fn hints_are_checked_against_the_real_default_value() {
        assert_eq!(hint("min_times", Some(&Value::Integer(2))), "");
        assert_eq!(
            hint("brief_times", Some(&Value::Array(vec![Value::String("12:50:00".into())]))),
            "每项 HH:MM:SS"
        );
        assert_eq!(
            hint("daily_time", Some(&Value::String("08:20:00".into()))),
            "HH:MM 或 HH:MM:SS"
        );
        assert_eq!(hint("restart_command", Some(&Value::String(String::new()))), "");
        assert_eq!(hint("cooldown_seconds", Some(&Value::Integer(15))), "秒");
    }

    #[test]
    fn numbers_land_on_the_field_type_they_are_written_to() {
        assert_eq!(
            to_toml(&json!(2), Some(&Value::Float(3.0))).unwrap(),
            Value::Float(2.0)
        );
        assert_eq!(
            to_toml(&json!(2), Some(&Value::Integer(0))).unwrap(),
            Value::Integer(2)
        );
        assert_eq!(
            to_toml(&json!([1, 2]), Some(&Value::Array(vec![Value::Float(0.0)]))).unwrap(),
            Value::Array(vec![Value::Float(1.0), Value::Float(2.0)])
        );
        assert!(to_toml(&Json::Null, None).is_err());
    }

    #[test]
    fn raw_values_accept_both_documents_and_literals() {
        let table = parse_toml("[a]\nb = 1\n", Some(&Value::Table(Default::default()))).unwrap();
        assert!(table.get("a").is_some());
        assert_eq!(
            parse_toml("[1, 2]", Some(&Value::Array(vec![]))).unwrap(),
            Value::Array(vec![Value::Integer(1), Value::Integer(2)])
        );
        assert!(parse_toml("= =", Some(&Value::Array(vec![]))).is_err());
    }
}
