//! 把采集到的素材交给模型，换回一份结构化的画像；模型不接时用统计量兜底。
//!
//! 这一层只认两件事：**模型的输出必须是一个 JSON 对象**，以及**引语必须是原话**。
//! 前者靠宽松解析（模型喜欢在 JSON 外面裹一句「好的」或一层代码块），后者靠
//! 归一化比对——对不上就丢掉，宁可少一条引语，也不让报告里出现一句编出来的
//! 「他说过」。

use super::collect::Material;
use serde::Deserialize;

/// 报告主色。色板是固定的，模型只能从中挑，省得它挑出一组刺眼或看不清的组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    Amber,
    Rose,
    Mint,
    Indigo,
    Violet,
    Teal,
}

impl Accent {
    pub const ALL: [Accent; 6] = [
        Accent::Amber,
        Accent::Rose,
        Accent::Mint,
        Accent::Indigo,
        Accent::Violet,
        Accent::Teal,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Amber => "amber",
            Accent::Rose => "rose",
            Accent::Mint => "mint",
            Accent::Indigo => "indigo",
            Accent::Violet => "violet",
            Accent::Teal => "teal",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|accent| accent.name() == name)
    }

    /// 兜底选色：同一个用户每次都得到同一种颜色，换人换色。
    pub fn pick(seed: i64) -> Self {
        Self::ALL[(seed.unsigned_abs() as usize) % Self::ALL.len()]
    }

    /// 主色（深色主题用亮一档的色，浅色主题用深一档的）。
    pub fn hex(self, dark: bool) -> &'static str {
        match (self, dark) {
            (Accent::Amber, false) => "#A86400",
            (Accent::Amber, true) => "#DDBB74",
            (Accent::Rose, false) => "#C1355A",
            (Accent::Rose, true) => "#EE93AC",
            (Accent::Mint, false) => "#168668",
            (Accent::Mint, true) => "#72C9AE",
            (Accent::Indigo, false) => "#5268D8",
            (Accent::Indigo, true) => "#9AA8E8",
            (Accent::Violet, false) => "#7B4BC9",
            (Accent::Violet, true) => "#BFA2EE",
            (Accent::Teal, false) => "#0F7C8C",
            (Accent::Teal, true) => "#6FC3D0",
        }
    }

    /// 同色的 `r,g,b` 字面量，供 CSS 里调透明度用，省得写死多份色值。
    pub fn rgb(self, dark: bool) -> &'static str {
        match (self, dark) {
            (Accent::Amber, false) => "168,100,0",
            (Accent::Amber, true) => "221,187,116",
            (Accent::Rose, false) => "193,53,90",
            (Accent::Rose, true) => "238,147,172",
            (Accent::Mint, false) => "22,134,104",
            (Accent::Mint, true) => "114,201,174",
            (Accent::Indigo, false) => "82,104,216",
            (Accent::Indigo, true) => "154,168,232",
            (Accent::Violet, false) => "123,75,201",
            (Accent::Violet, true) => "191,162,238",
            (Accent::Teal, false) => "15,124,140",
            (Accent::Teal, true) => "111,195,208",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Trait {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub score: f64,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Quote {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub why: String,
}

/// 一份可以交给模板渲染的画像。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Persona {
    #[serde(default)]
    pub codename: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub traits: Vec<Trait>,
    #[serde(default)]
    pub interests: Vec<String>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub style: String,
    #[serde(default)]
    pub rhythm: String,
    #[serde(default)]
    pub quotes: Vec<Quote>,
    #[serde(default)]
    pub advice: String,
    #[serde(default)]
    pub accent: String,
    /// 这份画像是模型写的，还是从统计量拼出来的。
    #[serde(skip)]
    pub estimated: bool,
}

/// 各字段的字数上限。模型偶尔会无视字数要求，这里统一收口，
/// 免得一个超长字段把整张卡的版面顶乱。
mod limit {
    pub const CODENAME: usize = 8;
    pub const TAGLINE: usize = 24;
    pub const SUMMARY: usize = 96;
    pub const TRAIT_NAME: usize = 6;
    pub const TRAIT_NOTE: usize = 40;
    pub const INTEREST: usize = 12;
    pub const SECTION: usize = 64;
    pub const QUOTE: usize = 90;
    pub const QUOTE_WHY: usize = 28;
    pub const ADVICE: usize = 40;
    pub const MAX_TRAITS: usize = 5;
    pub const MAX_INTERESTS: usize = 8;
    pub const MAX_QUOTES: usize = 3;
}

/// 截到上限并补省略号。省略号前面不留空白，否则会变成「手机 root …」这种断口。
fn clip(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('…');
    out
}

/// 逐字比对用的归一化：只留字母数字与汉字，去掉标点、空白和大小写差异。
fn fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace() && !is_punctuation(*ch))
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

fn is_punctuation(ch: char) -> bool {
    const EXTRA: &str = "，。！？；：、“”‘’（）《》〈〉【】〔〕…—～·「」『』／＼｜＋－×÷＝＜＞";
    ch.is_ascii_punctuation() || EXTRA.contains(ch)
}

/// 宽松解析模型输出：取第一个花括号到最后一个花括号之间的内容。
///
/// 开 JSON 模式能把可用模型限制在支持该参数的那几个上，为了一个字段的整洁
/// 换掉整个模型池不划算，取花括号块就够了。
pub fn parse(raw: &str) -> anyhow::Result<Persona> {
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("模型没有返回 JSON：{}", clip(raw, 120)))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("模型返回的 JSON 不完整：{}", clip(raw, 120)))?;
    if end <= start {
        anyhow::bail!("模型返回的 JSON 不完整：{}", clip(raw, 120));
    }
    let persona: Persona = serde_json::from_str(&raw[start..=end])
        .map_err(|error| anyhow::anyhow!("模型返回的 JSON 解析失败：{error}"))?;
    Ok(persona)
}

impl Persona {
    /// 收口：字数、条数、分数范围，以及引语必须出自样本。
    pub fn sanitize(mut self, material: &Material) -> Self {
        self.codename = clip(&self.codename, limit::CODENAME);
        self.tagline = clip(&self.tagline, limit::TAGLINE);
        self.summary = clip(&self.summary, limit::SUMMARY);
        self.role = clip(&self.role, limit::SECTION);
        self.style = clip(&self.style, limit::SECTION);
        self.rhythm = clip(&self.rhythm, limit::SECTION);
        self.advice = clip(&self.advice, limit::ADVICE);

        self.traits = self
            .traits
            .into_iter()
            .filter(|item| !item.name.trim().is_empty())
            .take(limit::MAX_TRAITS)
            .map(|mut item| {
                item.name = clip(&item.name, limit::TRAIT_NAME);
                item.note = clip(&item.note, limit::TRAIT_NOTE);
                item.score = if item.score.is_finite() {
                    item.score.clamp(0.0, 100.0)
                } else {
                    0.0
                };
                item
            })
            .collect();

        let mut seen = std::collections::HashSet::new();
        self.interests = self
            .interests
            .into_iter()
            .map(|word| clip(&word, limit::INTEREST))
            .filter(|word| !word.is_empty())
            .filter(|word| seen.insert(word.clone()))
            .take(limit::MAX_INTERESTS)
            .collect();

        let samples: Vec<String> = material.samples.iter().map(|s| fingerprint(s)).collect();
        self.quotes = self
            .quotes
            .into_iter()
            .filter(|quote| {
                let finger = fingerprint(&quote.text);
                finger.chars().count() >= 4
                    && samples.iter().any(|sample| sample.contains(&finger))
            })
            .take(limit::MAX_QUOTES)
            .map(|mut quote| {
                quote.text = clip(&quote.text, limit::QUOTE);
                quote.why = clip(&quote.why, limit::QUOTE_WHY);
                quote
            })
            .collect();

        self
    }

    /// 主色：模型挑的在色板里就用它，否则按用户号定色。
    pub fn accent(&self, seed: i64) -> Accent {
        Accent::from_name(&self.accent).unwrap_or_else(|| Accent::pick(seed))
    }

    /// 模型完全没接上时的兜底画像：全部由统计量拼出来。
    ///
    /// 与其回一句「生成失败」，不如把数字本身排成一张能看的图——用户要的信息
    /// 大半都在数字里，缺的只是文字评论。
    pub fn from_stats(material: &Material) -> Self {
        let night = material.night_ratio();
        let media = material.media_ratio();
        let per_day = material.per_day();
        let avg = material.avg_len();
        let engaging = material.reply_ratio()
            + if material.total > 0 {
                material.kinds.at as f64 / material.total as f64
            } else {
                0.0
            };

        let traits = vec![
            meter("话量", (per_day / 40.0 * 100.0).min(99.0), "看平均每天发几条"),
            meter("夜行", night * 100.0, "0 点到 6 点的发言占比"),
            meter("图文", media * 100.0, "图片、表情包与小表情的占比"),
            meter(
                "长句",
                (avg / 40.0 * 100.0).min(99.0),
                "单条发言的平均字数",
            ),
            meter("爱接话", engaging * 100.0, "引用与 @ 别人的比例"),
        ]
        .into_iter()
        .map(|(name, score, note)| Trait {
            name: name.to_string(),
            score: score.round(),
            note: note.to_string(),
        })
        .collect();

        let longest: Vec<Quote> = {
            let mut sorted: Vec<&String> = material.samples.iter().collect();
            sorted.sort_by_key(|text| std::cmp::Reverse(text.chars().count()));
            sorted
                .into_iter()
                .take(2)
                .map(|text| Quote {
                    text: text.clone(),
                    why: "他写得最长的一条".to_string(),
                })
                .collect()
        };

        Self {
            codename: "只按数字画的一张".to_string(),
            tagline: format!("{} 天里说了 {} 句话", material.span_days(), material.total),
            summary: format!(
                "统计窗口内共 {} 条群聊发言，覆盖 {} 天，活跃 {} 天，平均每天 {:.1} 条。\
                 单条最长 {} 字，平均 {:.1} 字。",
                material.total,
                material.span_days(),
                material.active_days,
                per_day,
                material.longest,
                avg
            ),
            traits,
            interests: Vec::new(),
            role: format!(
                "在 {} 个群里说过话，最常出没的是「{}」。",
                material.groups.len(),
                material
                    .groups
                    .first()
                    .map(|group| group.name.as_str())
                    .unwrap_or("未知群")
            ),
            style: format!(
                "{} 的发言里带图片或表情，{} 的发言引用了别人。",
                percent(media),
                percent(material.reply_ratio())
            ),
            rhythm: format!(
                "{} 最活跃，夜间（0—6 点）占 {}。",
                hour_label(material.peak_hour()),
                percent(night)
            ),
            quotes: longest,
            advice: "这次模型没接上，先按数字给你画了一张，过会儿再试一次。".to_string(),
            accent: Accent::pick(material.user_id).name().to_string(),
            estimated: true,
        }
    }
}

fn meter<'a>(name: &'a str, score: f64, note: &'a str) -> (&'a str, f64, &'a str) {
    // 分数太贴近 0 时条形几乎看不见，也给不出信息量，抬到 4 起步。
    (name, score.clamp(4.0, 99.0), note)
}

pub fn percent(ratio: f64) -> String {
    format!("{:.0}%", ratio * 100.0)
}

/// 「23 点前后」这样的说法比「23:00」更像人话。
pub fn hour_label(hour: usize) -> String {
    let hour = hour % 24;
    match hour {
        0 => "午夜".to_string(),
        1..=4 => format!("凌晨 {hour} 点"),
        5..=7 => format!("清晨 {hour} 点"),
        8..=11 => format!("上午 {hour} 点"),
        12 => "中午".to_string(),
        13..=17 => format!("下午 {hour} 点"),
        18..=22 => format!("晚上 {hour} 点"),
        _ => "深夜 23 点".to_string(),
    }
}

pub fn weekday_label(weekday: usize) -> &'static str {
    const NAMES: [&str; 7] = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
    NAMES[weekday % 7]
}

const SYSTEM_PROMPT: &str = r#"你是群聊数据分析师，负责给一个 QQ 群成员写一份「用户画像」。

你只看得到这个人在群里说过的话和一组统计数字，据此推断他的表达习惯、关心的话题和他在群里的位置。

写法要求：
- 只写从发言里看得出来的东西。不评价外貌、性别、年龄、职业、地域、收入、健康和政治倾向，也不去猜这些；看不出就不写。
- 不要报告腔。不要写「该用户」「整体来看」「展现出」这类词，写成朋友之间看了会心一笑的大白话。
- 引语必须逐字出自下发的发言样本，一个字都不能改；找不到合适的就不给引语。
- 特质分数要拉开差距，别都堆在七八十分。
- 只输出一个 JSON 对象，不要代码块、不要解释、不要前后缀。

JSON 字段：
{
  "codename": "画像代号，2 到 6 个字，像外号或标签，要具体、有画面感，别用「热心群友」这种谁都能套的词",
  "tagline": "一句话概括这个人，不超过 20 字",
  "summary": "两三句话的总评，不超过 80 字",
  "traits": [{"name":"特质名，2 到 4 字","score":0 到 100 的整数,"note":"这个特质的依据，不超过 30 字"}],
  "interests": ["兴趣词，2 到 6 字，4 到 8 个，按重要程度排序"],
  "role": "他在群里扮演的角色，不超过 30 字",
  "style": "他的说话风格，不超过 40 字",
  "rhythm": "他的活跃节律，不超过 30 字",
  "quotes": [{"text":"逐字引用的一条发言","why":"为什么挑它，不超过 20 字"}],
  "advice": "想对这个人说的一句话，不超过 30 字，可以损一点但要善意",
  "accent": "从 amber / rose / mint / indigo / violet / teal 里选一个当报告主色"
}"#;

/// 组装下发给模型的素材。统计在前、样本在后，模型先拿到骨架再看原文。
pub fn user_prompt(material: &Material) -> String {
    let mut out = String::with_capacity(8_192);
    out.push_str("【对象】\n");
    out.push_str(&format!("群名片：{}\n", material.name));
    out.push_str(&format!("QQ：{}\n\n", material.user_id));

    out.push_str("【统计】\n");
    out.push_str(&format!(
        "- 群聊发言 {} 条，覆盖 {} 天（活跃 {} 天），平均每天 {:.1} 条\n",
        material.total,
        material.span_days(),
        material.active_days,
        material.per_day()
    ));
    out.push_str(&format!(
        "- 单条最长 {} 字，平均 {:.1} 字\n",
        material.longest,
        material.avg_len()
    ));
    out.push_str(&format!(
        "- 活跃时段：{} 前后最活跃；夜间（0—6 点）占 {}；最活跃的一天是{}\n",
        hour_label(material.peak_hour()),
        percent(material.night_ratio()),
        weekday_label(material.peak_weekday())
    ));
    out.push_str(&format!(
        "- 纯文字 {} 条；图片 {}、表情包 {}、小表情 {}、语音 {}、视频 {}\n",
        material.kinds.text,
        material.kinds.image,
        material.kinds.anim_emoji,
        material.kinds.face,
        material.kinds.voice,
        material.kinds.video
    ));
    out.push_str(&format!(
        "- 引用别人的消息 {} 次，@ 别人 {} 次\n",
        material.kinds.reply, material.kinds.at
    ));
    if !material.groups.is_empty() {
        let groups: Vec<String> = material
            .groups
            .iter()
            .map(|group| format!("{}（{} 条）", group.name, group.count))
            .collect();
        out.push_str(&format!("- 常在的群：{}\n", groups.join("、")));
    }
    if !material.words.is_empty() {
        let words: Vec<String> = material
            .words
            .iter()
            .take(18)
            .map(|(word, count)| format!("{word}×{count}"))
            .collect();
        out.push_str(&format!("- 高频词：{}\n", words.join("、")));
    }

    out.push_str("\n【发言样本】（按时间由近及远，长句已截断）\n");
    if material.samples.is_empty() {
        out.push_str("（没有可读的发言样本）\n");
    } else {
        for (index, sample) in material.samples.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", index + 1, sample));
        }
    }
    out
}

pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::portrait::collect::{GroupSlice, Kinds};

    fn material() -> Material {
        Material {
            user_id: 3373167460,
            name: "甲".into(),
            total: 400,
            first_time: 1_700_000_000,
            last_time: 1_700_000_000 + 86_400 * 29,
            active_days: 20,
            hour: {
                let mut hour = [0u64; 24];
                hour[23] = 90;
                hour[2] = 40;
                hour
            },
            weekday: {
                let mut weekday = [0u64; 7];
                weekday[4] = 120;
                weekday
            },
            groups: vec![GroupSlice {
                name: "测试群".into(),
                count: 300,
            }],
            kinds: Kinds {
                text: 300,
                image: 40,
                anim_emoji: 40,
                face: 10,
                voice: 5,
                video: 5,
                reply: 60,
                at: 20,
            },
            longest: 210,
            avg_len: 18.0,
            words: vec![("天气".into(), 12)],
            samples: vec![
                "今天这个雨下得没完没了".to_string(),
                "凌晨三点还在改代码，明天又要废了".to_string(),
            ],
        }
    }

    #[test]
    fn json_is_found_behind_fences_and_chatter() {
        let raw = "好的，这是画像：\n```json\n{\"codename\":\"夜行改稿人\",\"tagline\":\"白天潜水夜里冒泡\"}\n```\n希望有帮助";
        let persona = parse(raw).unwrap();
        assert_eq!(persona.codename, "夜行改稿人");
    }

    #[test]
    fn json_without_braces_is_an_error() {
        assert!(parse("我觉得他挺好的").is_err());
    }

    #[test]
    fn quotes_must_appear_in_the_samples() {
        let persona = Persona {
            quotes: vec![
                Quote {
                    text: "凌晨三点还在改代码，明天又要废了".into(),
                    why: "原话".into(),
                },
                Quote {
                    text: "我从来没说过这句话".into(),
                    why: "编的".into(),
                },
            ],
            ..Default::default()
        }
        .sanitize(&material());
        assert_eq!(persona.quotes.len(), 1);
        assert!(persona.quotes[0].text.starts_with("凌晨三点"));
    }

    /// 标点与空白的差别不该让一条真原话被误判成编的。
    #[test]
    fn quotes_tolerate_punctuation_differences() {
        let persona = Persona {
            quotes: vec![Quote {
                text: "今天这个雨，下得没完没了！".into(),
                why: "原话".into(),
            }],
            ..Default::default()
        }
        .sanitize(&material());
        assert_eq!(persona.quotes.len(), 1);
    }

    #[test]
    fn overlong_fields_are_clipped_and_scores_clamped() {
        let persona = Persona {
            codename: "这是一个特别特别长的代号".into(),
            traits: vec![
                Trait {
                    name: "话痨".into(),
                    score: 480.0,
                    note: "长".repeat(80),
                },
                Trait {
                    score: 50.0,
                    ..Default::default()
                },
            ],
            interests: vec!["同一个词".into(), "同一个词".into(), " ".into()],
            ..Default::default()
        }
        .sanitize(&material());
        assert_eq!(persona.codename.chars().count(), limit::CODENAME + 1);
        // 空名字的特质被丢掉。
        assert_eq!(persona.traits.len(), 1);
        assert_eq!(persona.traits[0].score, 100.0);
        assert!(persona.traits[0].note.chars().count() <= limit::TRAIT_NOTE + 1);
        assert_eq!(persona.interests, vec!["同一个词".to_string()]);
        // 省略号前不留空白，别剪出「手机 root …」这种断口。
        let clipped = Persona {
            interests: vec!["abcdefghijk lmnop".into()],
            ..Default::default()
        }
        .sanitize(&material());
        assert_eq!(clipped.interests, vec!["abcdefghijk…".to_string()]);
    }

    #[test]
    fn accent_uses_the_palette_and_falls_back_deterministically() {
        let chosen = Persona {
            accent: "rose".into(),
            ..Default::default()
        };
        assert_eq!(chosen.accent(1), Accent::Rose);
        let garbage = Persona {
            accent: "chartreuse".into(),
            ..Default::default()
        };
        assert_eq!(garbage.accent(1), Accent::pick(1));
        assert_eq!(garbage.accent(1), garbage.accent(1));
        assert_eq!(Accent::ALL.len(), 6);
    }

    #[test]
    fn the_statistical_fallback_is_readable_on_its_own() {
        let persona = Persona::from_stats(&material());
        assert!(persona.estimated);
        assert_eq!(persona.traits.len(), 5);
        assert!(persona.traits.iter().all(|t| (4.0..=99.0).contains(&t.score)));
        assert_eq!(persona.quotes.len(), 2);
        assert!(persona.summary.contains("400"));
        assert!(persona.rhythm.contains("深夜"));
        assert!(Accent::from_name(&persona.accent).is_some());
    }

    #[test]
    fn the_prompt_carries_both_numbers_and_samples() {
        let prompt = user_prompt(&material());
        assert!(prompt.contains("群聊发言 400 条"));
        assert!(prompt.contains("凌晨三点还在改代码"));
        assert!(prompt.contains("高频词"));
        assert!(system_prompt().contains("逐字"));
    }
}
