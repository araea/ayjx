//! 调度回归与可选真实模型试聊；所有场景均不向 QQ 发送测试消息。
use super::*;

/// 一个只说固定一句话的假模型端点。
///
/// agent 在进程内之后，「假模型」不再是一个假 CLI，而是一个 OpenAI 兼容的
/// HTTP 服务：每次请求记一笔，等 `release` 出现再回话——调度回归要的正是
/// 「上一轮还没收尾时新消息怎么排队」。
async fn fake_model(reply: &str) -> (String, std::path::PathBuf, std::path::PathBuf, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let dir = std::env::temp_dir().join(format!("ayjx-ambient-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&dir).unwrap();
    let started = dir.join("started");
    let release = dir.join("release");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let body = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "created": 1,
        "model": "fake",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": reply},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    })
    .to_string();

    let task_started = started.clone();
    let task_release = release.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let (started, release, body) = (task_started.clone(), task_release.clone(), body.clone());
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    return;
                }
                let mut length = 0;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
                let mut payload = vec![0; length];
                let _ = reader.read_exact(&mut payload).await;

                // 第一轮被 release 卡住，第二轮直接回话。
                std::fs::write(&started, "x").map(|_| ()).ok();
                for _ in 0..2000 {
                    if release.exists() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let _ = reader
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (base, started, release, task)
}

#[tokio::test]
async fn new_messages_drain_into_the_next_round_and_a_summon_skips_the_gate() {
    use crate::config::{AppConfig, build_config};
    use crate::event::{BotStatus, EventType, LoginUser};
    use std::sync::RwLock;

    let dir =
        crate::plugins::oai::agent::ScratchDir::under(&std::env::temp_dir(), "ambient-test").unwrap();
    let (base, started, release, server) =
        fake_model("[focus:{\"topic\":\"测试话题\",\"seconds\":30}]\n[silent]").await;
    // 第一轮的回话要等测试放行，第二轮（搭话指令）才不必再等。
    let _ = std::fs::remove_file(&release);
    let group = -8_000_001;
    let ambient = AmbientConfig {
        enabled: true,
        groups: vec![group],
        // 判定模型与发言模型都指向这个假端点：没有供应商前缀，走 oai 默认接口。
        gate_model: "fake-model".into(),
        reply_model: "fake-model".into(),
        debounce_seconds: 1,
        context_images: 0,
        // 调度回归与计价时段无关；钉死它，免得这个测试在工作日上午换一种行为。
        peak: super::peak::PeakConfig {
            mode: super::peak::Mode::Normal,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut config = AppConfig::default();
    config
        .plugins
        .insert("ambient".into(), build_config(ambient));
    let ctx = Context {
        event: EventType::Init,
        config: Arc::new(RwLock::new(config)),
        config_save_lock: Arc::new(tokio::sync::Mutex::new(())),
        db: sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
        scheduler: Arc::new(crate::scheduler::Scheduler::new()),
        matcher: Arc::new(crate::matcher::Matcher::new()),
        config_path: Arc::from("unused-ambient-test.toml"),
        bot: Arc::new(BotStatus {
            adapter: "satori-qq".into(),
            platform: "qq".into(),
            login_user: LoginUser {
                id: "10000".into(),
                ..Default::default()
            },
        }),
    };
    let writer = Arc::new(crate::adapters::satori::SatoriClient::console());
    let mgr = Arc::new(crate::plugins::oai::data::Manager::new(dir.path().to_path_buf()));
    {
        // 模型端点指向假服务；密钥随便填，它只被塞进 Authorization 头。
        let mut c = mgr.config.write().await;
        c.api_base = base;
        c.api_key = "test-only".into();
        mgr.save(&c);
    }
    setup(dir.path()).await.unwrap();
    let base = dir.path().to_path_buf();
    let turn = |id| Turn {
        user_id: 42,
        name: "群友".into(),
        text: "@你 测试话题".into(),
        elements: crate::message::Message::new(),
        images: vec![],
        message_id: id,
        mentions_me: true,
        from_me: false,
        at: chrono::Local::now().timestamp(),
    };
    window::with_group(group, |state| {
        *state = Default::default();
        assert!(state.receive(turn(1)));
    });
    let task = tokio::spawn({
        let (ctx, writer, mgr, base) =
            (ctx.clone(), writer.clone(), mgr.clone(), base.clone());
        async move { consider(&ctx, &writer, &mgr, group, &base).await.unwrap() }
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    window::with_group(group, |state| assert!(!state.receive(turn(2))));
    std::fs::write(&release, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    window::with_group(group, |state| {
        assert!(!state.running);
        assert!(!state.take_mention());
        assert!(state.active_focus().is_some());
        assert_eq!(state.spoken_last_hour(), 0);
    });
    // 搭话指令直接把这一批交给人格：判定那一步完全不发生——这里假端点什么模型都答，
    // 真去判定也会成功，而它走到人格那一轮并写下关注，说明指令确实绕过了判定。
    assert!(window::with_group(group, |state| state.summon()));
    let task = tokio::spawn({
        let (ctx, writer, mgr, base) =
            (ctx.clone(), writer.clone(), mgr.clone(), base.clone());
        async move { consider(&ctx, &writer, &mgr, group, &base).await.unwrap() }
    });
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    window::with_group(group, |state| {
        assert!(!state.running);
        assert!(!state.take_summon());
    });
    server.abort();
}

#[tokio::test]
#[ignore = "需要 AYJX_AMBIENT_LIVE_DATA、已配置的模型接口和网络；仅打印试聊，不发群消息"]
async fn live_persona_and_gate_dialogue() {
    let data = PathBuf::from(std::env::var("AYJX_AMBIENT_LIVE_DATA").unwrap());
    let mgr = crate::plugins::oai::data::Manager::new(data.clone());
    let credentials = mgr.config.read().await;
    let dir =
        crate::plugins::oai::agent::ScratchDir::under(&std::env::temp_dir(), "ambient-live").unwrap();
    setup(dir.path()).await.unwrap();
    let config = AmbientConfig {
        tools: "read".into(),
        context_images: 0,
        reply_timeout_seconds: 120,
        ..Default::default()
    };
    // 判定模型写成「供应商/模型」时，线上由 [oai.providers] 取接口；这里没有 Context，
    // 就用环境变量补上那一段，否则带前缀的模型名会被原样发给 oai 的默认接口。
    let (provider, gate_model) = crate::plugins::oai::utils::split_provider(&config.gate_model);
    let gate_base = std::env::var("AYJX_AMBIENT_LIVE_GATE_BASE")
        .unwrap_or_else(|_| credentials.api_base.clone());
    let gate_key = std::env::var("AYJX_AMBIENT_LIVE_GATE_KEY")
        .unwrap_or_else(|_| credentials.api_key.clone());
    assert!(
        provider.is_none() || std::env::var("AYJX_AMBIENT_LIVE_GATE_BASE").is_ok(),
        "判定模型 {} 带供应商前缀，请设置 AYJX_AMBIENT_LIVE_GATE_BASE / _KEY",
        config.gate_model
    );
    let group = -8_000_002;
    let mut turns = Vec::new();
    let mut state = window::GroupState::default();
    for (index, text) in [
        "今天吃面 先下了",
        "这游戏说好休闲种田 结果我天天上线打卡 比上班还准时",
        "你说得对 我还花钱买了加速上班",
        "先不聊了 我关游戏睡了 不用回",
        // 试那把「手术刀」：对「看透了」这类话不服，也借它看温柔那一面还在不在。
        "折腾这么久我算是把人性看透了",
        "算了 说这些也没用 反正没人真懂",
    ]
    .iter()
    .enumerate()
    {
        turns.push(Turn {
            user_id: 114514,
            name: "群友甲".into(),
            text: (*text).into(),
            elements: crate::message::Message::new(),
            images: vec![],
            message_id: index as i64 + 1,
            mentions_me: false,
            from_me: false,
            at: chrono::Local::now().timestamp(),
        });
        let scene = Scene::build(group, &config, &turns, state.rhythm());
        let verdict = gate::judge(
            &gate_base,
            &gate_key,
            &gate_model,
            &config,
            &turns,
            PERSONA,
            &scene,
            None,
        )
        .await
        .unwrap();
        // 试聊同时查看人格决定，即便筛选不放行；线上仍按分数筛选。
        let (provider, reply_model) = crate::plugins::oai::utils::split_provider(&config.reply_model);
        let reply_base = std::env::var("AYJX_AMBIENT_LIVE_GATE_BASE")
            .unwrap_or_else(|_| credentials.api_base.clone());
        let reply_key = std::env::var("AYJX_AMBIENT_LIVE_GATE_KEY")
            .unwrap_or_else(|_| credentials.api_key.clone());
        let _ = provider;
        let raw = speak::compose(
            &reply_base,
            &reply_key,
            &reply_model,
            dir.path(),
            &skill_dirs(dir.path()),
            PERSONA,
            &config,
            &Default::default(),
            Some(Duration::from_secs(40)),
            &turns,
            &[],
            Called::Ordinary,
            &scene,
            None,
        )
        .await
        .unwrap();
        let (body, focus) = attention::extract(&raw, &turns, config.focus_max_seconds);
        if let Some(focus) = focus {
            state.focus = focus;
        }
        println!(
            "场景 {}：{}\n判定 {:?}\n人格 {}\n",
            index + 1,
            text,
            verdict,
            raw
        );
        match pace::parse(&body, 3, config.split_chars) {
            pace::Speech::Silent => {}
            pace::Speech::Say(items) => {
                assert!(!items.is_empty());
                for item in items {
                    let text = plain_text(&item.message);
                    assert!(!text.contains("[focus:"));
                    turns.push(Turn {
                        user_id: 10000,
                        name: "我".into(),
                        text,
                        elements: crate::message::Message::new(),
                        images: vec![],
                        message_id: 0,
                        mentions_me: false,
                        from_me: true,
                        at: chrono::Local::now().timestamp(),
                    });
                }
                state.mark_spoke();
            }
        }
    }
    for (text, draft, expected) in [
        ("我还给它充钱 感谢老板给我上班机会", "这下付费上班了", true),
        (
            "刚才说错了 这游戏没有打卡奖励 这话题不聊了",
            "那你明天别忘了领打卡奖励",
            false,
        ),
    ] {
        let latest = [Turn {
            user_id: 114514,
            name: "群友甲".into(),
            text: text.into(),
            elements: crate::message::Message::new(),
            images: vec![],
            message_id: 9,
            mentions_me: false,
            from_me: false,
            at: chrono::Local::now().timestamp(),
        }];
        let verdict = gate::judge(
            &gate_base,
            &gate_key,
            &gate_model,
            &config,
            &latest,
            PERSONA,
            &Scene::build(group, &config, &latest, state.rhythm()),
            Some(draft),
        )
        .await
        .unwrap();
        println!("草稿检查：{draft} → {verdict:?}");
        assert_eq!(verdict.score >= 50, expected);
    }
}
