//! 调度回归与可选真实模型试聊；所有场景均不向 QQ 发送测试消息。
use super::*;

#[cfg(unix)]
#[tokio::test]
async fn silent_turn_still_drains_new_mentions_and_does_not_replay_them() {
    use crate::config::{AppConfig, build_config};
    use crate::event::{BotStatus, EventType, LoginUser};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::RwLock;

    let dir =
        super::super::pi_agent::ScratchDir::under(&std::env::temp_dir(), "ambient-test").unwrap();
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let node = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .unwrap();
    assert!(node.status.success());
    let command = dir.path().join("pi");
    std::fs::write(&command, format!(r##"#!{}
const fs = require('fs');
let input = '';
process.stdin.on('data', c => input += c);
process.stdin.on('end', () => {{
  fs.appendFileSync({started:?}, 'x');
  const timer = setInterval(() => {{
    if (!fs.existsSync({release:?})) return;
    clearInterval(timer);
    console.log(JSON.stringify({{type:'message_end', message:{{role:'assistant',stopReason:'stop',content:[{{type:'text',text:'[focus:{{"topic":"测试话题","seconds":30}}]\n[silent]'}}]}}}}));
  }}, 20);
}});
"##, String::from_utf8_lossy(&node.stdout).trim())).unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let group = -8_000_001;
    let oai = super::super::OaiConfig {
        pi_command: command.to_str().unwrap().into(),
        ambient: AmbientConfig {
            enabled: true,
            groups: vec![group],
            debounce_seconds: 1,
            context_images: 0,
            // 调度回归与计价时段无关；钉死它，免得这个测试在工作日上午换一种行为。
            peak: super::peak::PeakConfig {
                mode: super::peak::Mode::Normal,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut config = AppConfig::default();
    config.plugins.insert("oai".into(), build_config(oai));
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
    let mgr = Arc::new(super::super::data::Manager::new(dir.path().to_path_buf()));
    init(dir.path()).await.unwrap();
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
    let task = tokio::spawn(async move {
        consider(&ctx, &writer, &mgr, group).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    window::with_group(group, |state| assert!(!state.receive(turn(2))));
    std::fs::write(&release, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(started).unwrap(), "xx");
    window::with_group(group, |state| {
        assert!(!state.running);
        assert!(!state.take_mention());
        assert!(state.active_focus().is_some());
        assert_eq!(state.spoken_last_hour(), 0);
    });
}

#[tokio::test]
#[ignore = "需要 AYJX_AMBIENT_LIVE_DATA、已配置的 pi 和网络；仅打印试聊，不发群消息"]
async fn live_persona_and_gate_dialogue() {
    let data = PathBuf::from(std::env::var("AYJX_AMBIENT_LIVE_DATA").unwrap());
    let mgr = super::super::data::Manager::new(data.clone());
    let credentials = mgr.config.read().await;
    let dir =
        super::super::pi_agent::ScratchDir::under(&std::env::temp_dir(), "ambient-live").unwrap();
    init(dir.path()).await.unwrap();
    let config = AmbientConfig {
        tools: "read".into(),
        context_images: 0,
        reply_timeout_seconds: 120,
        ..Default::default()
    };
    // 判定模型写成「供应商/模型」时，线上由 [oai.providers] 取接口；这里没有 Context，
    // 就用环境变量补上那一段，否则带前缀的模型名会被原样发给 oai 的默认接口。
    let (provider, gate_model) = super::super::utils::split_provider(&config.gate_model);
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
        let raw = speak::compose(
            "pi",
            dir.path(),
            &skill_dirs(dir.path()),
            PERSONA,
            &config,
            Some(Duration::from_secs(40)),
            &turns,
            &[],
            false,
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
