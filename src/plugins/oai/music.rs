//! Suno 文生歌（中转站的 `/suno/*` 接口）。
//!
//! 房间模型关键字命中 `[oai] music_models`（默认 `suno`）的房间，提示词不再交给聊天
//! 补全，而是走 Suno：先 `POST /suno/submit/music` 拿任务号，再 `GET /suno/fetch/{id}`
//! 轮询到出歌。一次提交会返回两个版本，两个都发——封面走图片段，音频按
//! `[oai] music_send` 决定发语音气泡还是群文件。
//!
//! 房间的系统提示词仍然当风格前缀用（和图像房间同一套写法），用户那句话接在后面；
//! 提示词是歌词体裁（带 `[Verse]` 之类的段落标记）时改走自定义模式，否则走「灵感模式」
//! ——把整段文字当作创作描述交给 Suno，歌词由它自己写。

use super::logic::{Media, MediaMessage, Reply};
use super::types::{Agent, ChatMessage};
use anyhow::{Context as _, anyhow};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::time::Duration;

/// 默认走 Suno 的模型关键字（不区分大小写、子串匹配）。
/// 站点上加别的音乐服务商时改 `[oai] music_models` 即可。
pub(super) const DEFAULT_MUSIC_MODELS: &[&str] = &["suno"];

/// 兜底模型 id：模型列表还没拉回来（或列表里没有 Suno）时预设房间用它。
pub(super) const FALLBACK_MODEL: &str = "suno_music";

/// 兜底版本号。中转站实际把 `chirp-v5` 映射到当前最新模型（返回里是 `v6` / `chirp-hawk`），
/// 所以写 `chirp-v5` 拿到的就是最"新"的那一档；站点接上更新的版本时改 `[oai] music_version`。
pub(super) const DEFAULT_VERSION: &str = "chirp-v5";

const POLL_INTERVAL: Duration = Duration::from_secs(6);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// 一首歌最多等多久；真正的总预算由 `[oai] media_timeout_seconds` 兜底。
const TASK_TIMEOUT: Duration = Duration::from_secs(8 * 60);
/// 一次提交最多发几首（Suno 一次给两个版本）。
const MAX_CLIPS: usize = 2;
/// 回复里最多带几行歌词当引子。
const LYRICS_LINES: usize = 4;

/// 音频怎么发进群。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendMode {
    /// 只发群文件。
    File,
    /// 只发语音气泡。
    Voice,
    /// 先发群文件（能存能下载），再补一条语音气泡。
    Both,
}

impl SendMode {
    pub(super) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "voice" | "语音" => Self::Voice,
            "file" | "文件" | "群文件" => Self::File,
            _ => Self::Both,
        }
    }

    fn file(self) -> bool {
        matches!(self, Self::File | Self::Both)
    }

    fn voice(self) -> bool {
        matches!(self, Self::Voice | Self::Both)
    }
}

/// 模型是否走 Suno 接口。`keywords` 为空时视为不启用。
pub(crate) fn is_music_model(model: &str, keywords: &[String]) -> bool {
    let lower = model.trim().to_lowercase();
    keywords
        .iter()
        .filter(|keyword| !keyword.trim().is_empty())
        .any(|keyword| lower.contains(&keyword.trim().to_lowercase()))
}

/// 一次生成的产物：若干版本的歌。
pub(super) struct Generated {
    lyrics: String,
    tags: String,
    version: String,
    cost: f64,
    clips: Vec<Clip>,
}

/// 去掉参数后剩下的正文，以及几个可选项。
#[derive(Debug, Default, PartialEq)]
struct Options {
    prompt: String,
    title: String,
    tags: String,
    version: String,
    instrumental: bool,
}

/// 从提示词里剥离 `--标题/--title`、`--风格/--tags`、`--版本/--mv` 与 `--纯音乐/--instrumental`。
/// 参数缺值时原样留在正文里，避免把用户想写的东西悄悄吃掉。
fn parse_options(input: &str) -> Options {
    let mut words: Vec<&str> = Vec::new();
    let mut options = Options::default();
    let mut tokens = input.split_whitespace().peekable();

    while let Some(token) = tokens.next() {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "--标题" | "--title" | "--歌名" => match tokens.peek() {
                Some(value) => {
                    options.title = (*value).to_string();
                    tokens.next();
                }
                None => words.push(token),
            },
            "--风格" | "--tags" | "--tag" => match tokens.peek() {
                Some(value) => {
                    options.tags = (*value).to_string();
                    tokens.next();
                }
                None => words.push(token),
            },
            "--版本" | "--mv" => match tokens.peek() {
                Some(value) => {
                    options.version = (*value).to_string();
                    tokens.next();
                }
                None => words.push(token),
            },
            "--纯音乐" | "--instrumental" | "--instrument" => options.instrumental = true,
            _ => words.push(token),
        }
    }

    options.prompt = words.join(" ");
    options
}

/// 正文是不是一段写好的歌词。Suno 的段落标记是 `[Verse]` / `[Chorus]` 这一族；
/// 中文写法（`[主歌]` / `[副歌]`）一并认。
fn looks_like_lyrics(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "[verse", "[chorus", "[bridge", "[intro", "[outro", "[pre-chorus", "[hook", "[主歌",
        "[副歌", "[间奏", "[尾声",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// 最后一条用户消息就是本轮提示词；房间系统提示词是风格前缀。
fn last_user(hist: &[ChatMessage]) -> Option<&ChatMessage> {
    hist.iter().rev().find(|message| message.role == "user")
}

/// 调用 Suno，把结果包装成与聊天补全一致的 `Reply`（正文是歌名/时长/歌词引子，
/// 封面与音频放进 `media` 由发送阶段按段发出去）。
pub(super) async fn generate_reply(
    api_base: &str,
    api_key: &str,
    agent: &Agent,
    hist: &[ChatMessage],
    config: &super::OaiConfig,
) -> anyhow::Result<Reply> {
    let last = last_user(hist);
    let options = parse_options(
        &match (agent.system_prompt.trim(), last.map(|m| m.content.as_str()).unwrap_or("")) {
            ("", user) => user.to_string(),
            (system, "") => system.to_string(),
            (system, user) => format!("{system}\n{user}"),
        },
    );
    let prompt = options.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(anyhow!(
            "请说明想要一首什么样的歌，例如：唱一首关于秋天的民谣"
        ));
    }

    let version = if options.version.trim().is_empty() {
        config.music_version()
    } else {
        options.version.trim().to_string()
    };
    let generated = generate(
        api_base,
        api_key,
        &prompt,
        &version,
        &options,
        config.media_timeout(),
    )
    .await?;

    let title = generated
        .clips
        .iter()
        .map(|clip| clip.title.trim())
        .find(|title| !title.is_empty())
        .unwrap_or("生成完成")
        .to_string();
    let durations: Vec<String> = generated
        .clips
        .iter()
        .map(|clip| mmss(clip.duration))
        .collect();

    let mut text = format!("🎵 **{title}** · {}", durations.join(" / "));
    // 新版 Suno 回的 tags 是一整段风格描述，整段贴进群太长，截一行够看就行。
    let tags = super::utils::truncate_str(generated.tags.trim(), 80);
    if !tags.is_empty() {
        text.push('\n');
        text.push_str(&tags);
    }
    if !generated.version.trim().is_empty() {
        text.push_str(&format!(" · Suno {}", generated.version.trim()));
    }
    if generated.cost > 0.0 {
        text.push_str(&format!(" · ${:.2}", generated.cost));
    }
    let lyrics = lyrics_excerpt(&generated.lyrics);
    if !lyrics.is_empty() {
        text.push_str("\n\n");
        text.push_str(&lyrics);
    }

    Ok(Reply {
        text,
        sources: Vec::new(),
        trace: Vec::new(),
        trace_overflow: 0,
        model: Some(agent.model.clone()),
        plain: true,
        media: media_messages(&generated.clips, &title, config.music_send()),
    })
}

/// 每个版本一条消息：封面 + 音频。`both` 模式再为每一版补一条语音气泡。
fn media_messages(clips: &[Clip], title: &str, mode: SendMode) -> Vec<MediaMessage> {
    let name = format!("{}.mp3", safe_name(title));
    let mut messages = Vec::new();
    for clip in clips {
        let audio = clip.audio_url.trim();
        if audio.is_empty() {
            continue;
        }
        let mut segments = Vec::new();
        if !clip.image_url.trim().is_empty() {
            segments.push(Media::Image {
                url: clip.image_url.trim().to_string(),
            });
        }
        if mode.file() {
            segments.push(Media::File {
                url: audio.to_string(),
                name: name.clone(),
            });
        } else if mode.voice() {
            // 只发语音时把语音放前半条，封面跟着一起走。
            segments.push(Media::Audio {
                url: audio.to_string(),
            });
        }
        messages.push(MediaMessage { segments });
        if mode == SendMode::Both {
            messages.push(MediaMessage {
                segments: vec![Media::Audio {
                    url: audio.to_string(),
                }],
            });
        }
    }
    messages
}

/// 歌词引子：最多几行，末尾空行与段落标记丢掉，让回复读起来像人写的摘要。
fn lyrics_excerpt(lyrics: &str) -> String {
    let lines: Vec<&str> = lyrics
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !(line.starts_with('[') && line.ends_with(']')))
        .take(LYRICS_LINES)
        .collect();
    lines.join("\n")
}

fn mmss(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// 文件名里的路径分隔符与引号一律换掉，QQ 群文件列表才不会看着乱。
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "suno".to_string()
    } else {
        super::utils::truncate_str(&cleaned, 60)
    }
}

/// 提交一次生成并等到出结果。
async fn generate(
    api_base: &str,
    api_key: &str,
    prompt: &str,
    version: &str,
    options: &Options,
    deadline: Duration,
) -> anyhow::Result<Generated> {
    let base = service_base(api_base);
    let custom = looks_like_lyrics(prompt);
    let body = json!({
        "prompt": if custom { prompt } else { "" },
        "tags": options.tags.trim(),
        "title": options.title.trim(),
        "mv": version,
        "make_instrumental": options.instrumental,
        "gpt_description_prompt": if custom { "" } else { prompt },
        "task_id": "",
        "continue_at": 0,
        "continue_clip_id": "",
        "notify_hook": "",
    });

    let task_id = submit(&base, api_key, &body).await?;
    info!(
        target: "Plugin/OAI/Music",
        "Suno 任务 {} 已提交（{}，{}模式）",
        task_id,
        version,
        if custom { "自定义" } else { "灵感" }
    );
    let task = poll(&base, api_key, &task_id, deadline).await?;
    if task.clips.is_empty() {
        return Err(anyhow!("Suno 任务已完成，但没有返回任何音频"));
    }
    Ok(Generated {
        lyrics: task
            .clips
            .first()
            .map(|clip| clip.prompt.clone())
            .unwrap_or_default(),
        tags: task
            .clips
            .first()
            .map(|clip| clip.tags.clone())
            .unwrap_or_default(),
        version: task
            .clips
            .first()
            .map(|clip| clip.model_version())
            .filter(|version| !version.trim().is_empty())
            .unwrap_or_else(|| version.to_string()),
        cost: task.cost,
        clips: task.clips.into_iter().take(MAX_CLIPS).collect(),
    })
}

/// Suno 的接口在服务根路径下（`/suno/...`），而房间配的是 OpenAI 兼容的 `.../v1`。
fn service_base(configured: &str) -> String {
    let base = configured.trim().trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).trim_end_matches('/').to_string()
}

async fn submit(base: &str, key: &str, body: &Value) -> anyhow::Result<String> {
    let endpoint = format!("{base}/suno/submit/music");
    let response = crate::http::client()
        .post(&endpoint)
        .bearer_auth(key)
        .json(body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("提交 {endpoint} 失败"))?;
    let status = response.status();
    let bytes = response.bytes().await.context("读取提交响应失败")?;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if code.eq_ignore_ascii_case("success") {
        return scalar_string(value.get("data"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("提交成功但没有返回任务 ID"));
    }
    if !status.is_success() || !code.is_empty() {
        let detail = if message.trim().is_empty() {
            excerpt(&bytes)
        } else {
            message.to_string()
        };
        return Err(anyhow!("Suno 提交失败（{}）：{}", status.as_u16(), detail));
    }
    Err(anyhow!("Suno 提交失败：{}", excerpt(&bytes)))
}

#[derive(Debug, Default, Deserialize)]
struct Task {
    #[serde(default, deserialize_with = "null_default")]
    status: String,
    #[serde(default, deserialize_with = "null_default")]
    fail_reason: String,
    #[serde(default, deserialize_with = "null_default")]
    progress: String,
    /// 任务产物：`MUSIC` 是曲子数组，生成歌词时是单个对象；这里只取数组。
    #[serde(default, rename = "data", deserialize_with = "null_default")]
    clips: Vec<Clip>,
    #[serde(default)]
    cost: f64,
}

impl Task {
    fn done(&self) -> Option<Result<(), String>> {
        match self.status.to_ascii_uppercase().as_str() {
            "SUCCESS" => Some(Ok(())),
            "FAILURE" => Some(Err(if self.fail_reason.trim().is_empty() {
                "Suno 任务执行失败".to_string()
            } else {
                self.fail_reason.trim().to_string()
            })),
            _ if self.progress.trim() == "100%" => Some(Ok(())),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Clip {
    #[serde(default, deserialize_with = "null_default")]
    audio_url: String,
    #[serde(default, deserialize_with = "null_default")]
    image_url: String,
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    tags: String,
    #[serde(default, deserialize_with = "null_default")]
    prompt: String,
    #[serde(default, deserialize_with = "null_default")]
    major_model_version: String,
    #[serde(default, deserialize_with = "null_default")]
    model_name: String,
    #[serde(default, deserialize_with = "null_default")]
    duration: f64,
}

impl Clip {
    /// 回复里写的版本：优先人类看的 `v6`，没有再退到渠道名 `chirp-hawk`。
    fn model_version(&self) -> String {
        if !self.major_model_version.trim().is_empty() {
            self.major_model_version.trim().to_string()
        } else {
            self.model_name.trim().to_string()
        }
    }
}

async fn poll(base: &str, key: &str, task_id: &str, deadline: Duration) -> anyhow::Result<Task> {
    let endpoint = format!("{base}/suno/fetch/{task_id}");
    let stop = tokio::time::Instant::now() + TASK_TIMEOUT.min(deadline);
    loop {
        let response = crate::http::client()
            .get(&endpoint)
            .bearer_auth(key)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .context("查询 Suno 任务失败")?;
        let status = response.status();
        let bytes = response.bytes().await.context("读取 Suno 任务响应失败")?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let code = value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !status.is_success() || !code.eq_ignore_ascii_case("success") {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("");
            return Err(anyhow!(
                "查询 Suno 任务失败（{}）：{}",
                status.as_u16(),
                if message.is_empty() {
                    excerpt(&bytes)
                } else {
                    message.to_string()
                }
            ));
        }
        let task: Task = serde_json::from_value(value.get("data").cloned().unwrap_or(Value::Null))
            .context("无法解析 Suno 任务响应")?;
        if let Some(result) = task.done() {
            return match result {
                Ok(()) => Ok(task),
                Err(reason) => Err(anyhow!(reason)),
            };
        }
        if tokio::time::Instant::now() >= stop {
            return Err(anyhow!("等了太久还没出歌（任务 {task_id}）"));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(value) = value.as_str() {
        Some(value.to_string())
    } else if let Some(value) = value.as_i64() {
        Some(value.to_string())
    } else {
        value.as_u64().map(|value| value.to_string())
    }
}

fn excerpt(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_configured_keywords_case_insensitively() {
        let keywords = vec!["suno".to_string()];
        assert!(is_music_model("suno_music", &keywords));
        assert!(is_music_model("SUNO-lyrics", &keywords));
        assert!(!is_music_model("gpt-image-2.5-flare", &keywords));
        assert!(!is_music_model("suno_music", &[]));
    }

    #[test]
    fn parses_and_strips_music_flags() {
        let options = parse_options("一首秋天的歌 --标题 落叶 --风格 folk,acoustic --版本 chirp-v5");
        assert_eq!(options.prompt, "一首秋天的歌");
        assert_eq!(options.title, "落叶");
        assert_eq!(options.tags, "folk,acoustic");
        assert_eq!(options.version, "chirp-v5");
        assert!(!options.instrumental);
    }

    #[test]
    fn keeps_valueless_flags_and_reads_instrumental() {
        let options = parse_options("钢琴小品 --纯音乐 --title");
        assert_eq!(options.prompt, "钢琴小品 --title");
        assert!(options.instrumental);
        assert!(options.title.is_empty());
    }

    #[test]
    fn recognizes_written_lyrics() {
        assert!(looks_like_lyrics("[Verse 1]\nSun on the table"));
        assert!(looks_like_lyrics("[副歌]\n啦啦啦"));
        assert!(!looks_like_lyrics("唱一首关于秋天的民谣"));
    }

    #[test]
    fn stems_the_voice_and_file_socket_from_the_url() {
        assert_eq!(service_base("https://api.apilio.ai/v1/"), "https://api.apilio.ai");
        assert_eq!(service_base("https://api.apilio.ai/v1"), "https://api.apilio.ai");
        assert_eq!(service_base("https://api.apilio.ai"), "https://api.apilio.ai");
    }

    #[test]
    fn send_mode_parses_with_a_permissive_default() {
        assert_eq!(SendMode::parse("voice"), SendMode::Voice);
        assert_eq!(SendMode::parse("文件"), SendMode::File);
        assert_eq!(SendMode::parse("both"), SendMode::Both);
        // 写错的值当成 both：宁可多发一条，也别把歌吞掉。
        assert_eq!(SendMode::parse("whatever"), SendMode::Both);
    }

    #[test]
    fn builds_one_message_per_clip_with_the_cover_up_front() {
        let clips = vec![
            Clip {
                audio_url: "https://a/1.mp3".into(),
                image_url: "https://a/1.jpg".into(),
                title: "落叶".into(),
                duration: 143.2,
                ..Default::default()
            },
            Clip {
                audio_url: "https://a/2.mp3".into(),
                image_url: "https://a/2.jpg".into(),
                title: "落叶".into(),
                duration: 61.0,
                ..Default::default()
            },
        ];
        let messages = media_messages(&clips, "落叶", SendMode::File);
        assert_eq!(messages.len(), 2);
        assert!(matches!(messages[0].segments[0], Media::Image { .. }));
        assert!(matches!(
            &messages[0].segments[1],
            Media::File { name, .. } if name == "落叶.mp3"
        ));
        // both：每版再补一条语音。
        assert_eq!(media_messages(&clips, "落叶", SendMode::Both).len(), 4);
        // voice：不重复发文件。
        let voice = media_messages(&clips, "落叶", SendMode::Voice);
        assert_eq!(voice.len(), 2);
        assert!(matches!(voice[0].segments[1], Media::Audio { .. }));
    }

    #[test]
    fn formats_durations_and_lyrics_as_a_short_lead_in() {
        assert_eq!(mmss(143.2), "2:23");
        assert_eq!(mmss(0.0), "0:00");
        assert_eq!(
            lyrics_excerpt("[Verse 1]\nSun on the table\n\nSteam in my mug\n[Chorus]"),
            "Sun on the table\nSteam in my mug"
        );
    }

    #[test]
    fn parses_a_finished_task_and_its_status_states() {
        let task: Task = serde_json::from_value(json!({
            "status": "SUCCESS",
            "fail_reason": "",
            "progress": "100%",
            "cost": 0.5,
            "data": [
                {"audio_url": "https://a/1.mp3", "image_url": null, "title": "落叶",
                 "duration": 143.2, "prompt": "[Verse]\nSun", "major_model_version": "v6"},
                {"audio_url": "https://a/2.mp3", "title": null, "duration": 134.4}
            ]
        }))
        .unwrap();
        assert!(task.done().unwrap().is_ok());
        assert_eq!(task.clips.len(), 2);
        assert_eq!(task.clips[0].model_version(), "v6");
        assert!(task.clips[1].title.is_empty());
        assert!((task.cost - 0.5).abs() < f64::EPSILON);

        let running: Task = serde_json::from_value(json!({"status": "IN_PROGRESS"})).unwrap();
        assert!(running.done().is_none());
        let failed: Task =
            serde_json::from_value(json!({"status": "FAILURE", "fail_reason": "上游超时"})).unwrap();
        assert_eq!(failed.done().unwrap().unwrap_err(), "上游超时");
    }

    #[test]
    fn sanitizes_the_group_file_name() {
        assert_eq!(safe_name("落叶 / 秋"), "落叶 _ 秋");
        assert_eq!(safe_name("   "), "suno");
        assert_eq!(safe_name("."), "suno");
    }
}
