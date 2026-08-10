// 此文件由 src/plugins.rs 包含，用于管理插件注册列表。
// 若要添加新插件，请确保 src/plugins/ 目录下存在对应的 .rs 文件，
// 并在下方列表中添加插件名称。

register_plugins!(
    filter_meta_event,
    logger,
    recorder {
        on_init: Some(recorder::init)
    },
    media_transfer,
    sticker_saver,
    group_self_title,
    ping_pong {
        on_init: Some(ping_pong::init)
    },
    recall,
    echo,
    repeater,
    word_cloud,
    stats_visualizer {
        on_connected: Some(stats_visualizer::on_connected)
    },
    card_reader,
    gif_lab,
    image_splitter,
    ciyi {
        on_init: Some(ciyi::init)
    },
    web_shot,
    shindan {
        on_init: Some(shindan::init)
    },
    oai {
        on_init: Some(oai::init)
    },
    help,
    auto_restart {
        on_init: Some(auto_restart::init)
    },
);
