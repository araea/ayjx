//! 画图预设房间：把公开分享的那些「一句话就出好图」的提示词做成开箱即用的房间。
//!
//! 图像房间的规则很简单：房间的系统提示词是风格前缀，用户那句话接在后面，两段一起
//! 交给图像接口（见 [`super::images::generate_reply`]）。于是「预设」不需要任何新机制
//! ——它就是一间系统提示词写得足够好的房间。这里只做三件事：备好那些提示词、
//! 首次启动时把它们建成房间、给它们一个不会被误触发的名字。
//!
//! **名字**一律是 `画·<两三个字>`。房间指令是前缀匹配的（见 `parser::parse_agent_cmd`），
//! 房间叫「手办」就意味着任何以「手办」开头的闲聊都会被当成绘图指令；中间那个
//! `·` 让误触发几乎不可能发生，又不影响中文输入法直接打出来。
//!
//! **删掉的不会复活**：建过的名字记在 `seeded_presets` 里，管理员删掉哪间就是不要哪间。
//! 新增预设只会补建没见过的那几间，不动已有房间的模型和提示词。

use super::types::{Agent, Config, ENGINE_CHAT};

/// 这些房间在 `/#` 列表里单独成区。
pub(crate) const SECTION: &str = "画图预设";

/// 预设房间名的前缀。挡住误触发，也让它们在列表里排在一起。
pub(crate) const PREFIX: &str = "画·";

/// 预设房间优先选用的模型关键字；站点上有多个同系列 id 时取第一个。
const MODEL_KEYWORD: &str = "gpt-image-2.5";

/// 模型列表还没拉回来时用的兜底 id。
const FALLBACK_MODEL: &str = "gpt-image-2.5-flare";

/// 音乐预设独立成区，名字也用另一个前缀：`歌·`。
///
/// 三串预设共用同一套机制（系统提示词当风格前缀），只是模型落到不同的专用接口上，
/// 所以各自一个前缀与分区，翻 `/#` 时一眼能分出这是画图、写歌还是拍片。
pub(crate) const MUSIC_SECTION: &str = "音乐房间";
pub(crate) const MUSIC_PREFIX: &str = "歌·";
pub(crate) const VIDEO_SECTION: &str = "视频房间";
pub(crate) const VIDEO_PREFIX: &str = "影·";

/// 接在音乐风格提示词后面的一句。用户写中文主题、Suno 出中文歌，所以让它跟着用户的
/// 语言走；风格那半段用英文写（Suno 对英文风格描述更稳）。
const MUSIC_BRIDGE: &str = "\nWrite one complete song in the style above. The user's line below is what the song is about; write the lyrics in the language the user wrote in, and keep any words the user gave verbatim.";

/// 接在视频风格提示词后面的一句。
const VIDEO_BRIDGE: &str = "\nShoot one short clip in the style above. The user's line below is what happens on screen; keep it literal, and do not burn in captions or subtitles.";

/// 一间预设房间：画图、写歌、拍片都是同一个形状，差别只在模型落到哪个专用接口上，
/// 以及接在提示词后面的那句 `BRIDGE`。
pub(crate) struct MediaPreset {
    /// 房间名，含对应分区的前缀（`画·` / `歌·` / `影·`）。
    pub name: &'static str,
    /// 列表里那一行说明。
    pub desc: &'static str,
    /// 风格提示词；对应的 `BRIDGE` 会接在它后面。
    pub prompt: &'static str,
}

/// 写歌预设。`--纯音乐` 这类开关写在提示词里，与用户那句话里的开关同一套解析。
pub(crate) const MUSIC_PRESETS: &[MediaPreset] = &[
    MediaPreset {
        name: "歌·随写",
        desc: "一句话生成一首歌",
        prompt: "A catchy, well-produced song: a memorable melody, a clear structure (verse, chorus, bridge), tasteful arrangement and a clean mix.",
    },
    MediaPreset {
        name: "歌·国风",
        desc: "国风：古筝笛子配现代编曲",
        prompt: "Chinese style (guofeng) pop: guzheng, dizi and erhu over a modern arrangement, pentatonic melodies, elegant imagery in the lyrics, a restrained and graceful mood.",
    },
    MediaPreset {
        name: "歌·民谣",
        desc: "民谣：木吉他、近麦人声",
        prompt: "Acoustic folk: fingerpicked guitar, warm close-mic vocals, light hand percussion, an intimate unhurried tempo, plain and honest lyrics.",
    },
    MediaPreset {
        name: "歌·电子",
        desc: "电子舞曲：四拍底鼓与合成器",
        prompt: "Upbeat electronic dance pop: a punchy four-on-the-floor kick, bright synth leads, sidechained pads, a big energetic drop.",
    },
    MediaPreset {
        name: "歌·说唱",
        desc: "中文说唱：boombap 鼓组",
        prompt: "Chinese hip-hop: boom-bap drums, a deep 808 bass, a confident laid-back flow, a memorable hook.",
    },
    MediaPreset {
        name: "歌·纯音乐",
        desc: "纯音乐，全程无人声",
        prompt: "An instrumental piece with a strong main melody and a clear arc from soft to full.\n--纯音乐",
    },
];

/// 拍片预设。
pub(crate) const VIDEO_PRESETS: &[MediaPreset] = &[
    MediaPreset {
        name: "影·通用",
        desc: "按你描述拍一段",
        prompt: "Cinematic footage with natural camera movement, believable physics and realistic lighting, high detail.",
    },
    MediaPreset {
        name: "影·电影感",
        desc: "电影质感与运镜",
        prompt: "A cinematic film shot: 35mm lens, shallow depth of field, motivated practical lighting, subtle film grain, one smooth deliberate camera move, colour graded like a prestige drama.",
    },
    MediaPreset {
        name: "影·动画",
        desc: "手绘二维动画风",
        prompt: "Hand-drawn 2D animation: expressive characters, clean line art, painted backgrounds, lively exaggeration, traditional cel-animation look.",
    },
    MediaPreset {
        name: "影·广告",
        desc: "广告级产品镜头",
        prompt: "A polished TV commercial shot: bright controlled product lighting, smooth dolly and orbit moves, glossy surfaces, clean composition, high production value.",
    },
];

/// 接在每份风格提示词后面的一句，把用户那半句话接上。
///
/// 用户可能什么都不写只发一张图（「画·手办」+ 一张照片），也可能只写一句描述，
/// 所以这句话两边都要兜住，且不能反过来盖掉风格。
const BRIDGE: &str = "\nIf reference images are provided, keep the subject in them recognizable and apply the style above to it; otherwise create the subject described below. Follow any extra instruction in the user's text, but keep the style above.\n以下是这次要画的东西：";

/// 预设清单。提示词取自公开分享的绘图 prompt，改写成「风格前缀 + 用户描述」的形状。
pub(crate) const PRESETS: &[MediaPreset] = &[
    MediaPreset {
        name: "画·手办",
        desc: "手办化：1/7 比例 PVC 模型摆拍",
        prompt: "Turn the subject into a high-quality 1/7 scale collectible PVC figure standing on a round transparent acrylic base, photographed on a tidy modern desk. Behind it sits the printed retail box showing the same character as key art, and a computer monitor displaying the ZBrush sculpting process of that figure. Realistic plastic and fabric materials, soft studio lighting, shallow depth of field, product photography.",
    },
    MediaPreset {
        name: "画·毛绒",
        desc: "毛绒玩偶化：布料质感与针脚",
        prompt: "Turn the subject into an adorable handmade plush toy: chunky rounded proportions, visible fuzzy fabric texture, neat stitching seams, embroidered eyes and nose, a small cloth tag on the side. Sitting on a soft bed in warm window light, shallow depth of field, cozy lifestyle photo.",
    },
    MediaPreset {
        name: "画·乐高",
        desc: "乐高小人仔与盒装套组",
        prompt: "Turn the subject into a LEGO minifigure with brick-built accessories that match its character, standing on a LEGO baseplate in front of its own boxed set; the retail box shows the character artwork and a collector badge. Glossy ABS plastic, crisp studio product photography, bright even lighting, clean background.",
    },
    MediaPreset {
        name: "画·黏土",
        desc: "黏土定格动画风，手作痕迹",
        prompt: "Render the subject as a handmade polymer clay stop-motion character: visible fingerprints and sculpting tool marks, slightly imperfect shapes, felt and cardboard set pieces around it, tilt-shift miniature feel, warm practical lighting, the charm of a frame from a stop-motion short film.",
    },
    MediaPreset {
        name: "画·像素",
        desc: "16 位像素风游戏画面",
        prompt: "Render the subject as 16-bit era pixel art: a limited 32-color palette, crisp readable silhouette, dithered shading, dark outlines, no anti-aliasing, placed in a side-scrolling game scene with a parallax background and a small HUD in the corner.",
    },
    MediaPreset {
        name: "画·微缩",
        desc: "等距微缩场景，玻璃罩里的小世界",
        prompt: "Render the subject as an isometric miniature diorama inside a glass display cube: tiny highly detailed props and figures, tilt-shift miniature effect, soft volumetric light, 45-degree isometric camera, clean neutral background, crisp 3D render.",
    },
    MediaPreset {
        name: "画·剧照",
        desc: "电影剧照质感的人像",
        prompt: "A cinematic film still of the subject: 85mm lens, shallow depth of field, motivated practical lighting with a strong key and soft fill, subtle film grain, teal and amber color grade, gentle anamorphic flare, 2.39:1 framing, composed like a frame from a prestige drama.",
    },
    MediaPreset {
        name: "画·证件",
        desc: "正装职业头像 / 证件照",
        prompt: "A professional headshot of the subject: neutral light-gray seamless backdrop, business attire, large soft key light with gentle fill, clear catchlights in the eyes, natural skin texture preserved, sharp focus on the eyes, 85mm f/2.8 look, suitable for a résumé or a company profile page. Keep the person's facial identity and features unchanged.",
    },
    MediaPreset {
        name: "画·水彩",
        desc: "手绘水彩插画，留白干净",
        prompt: "A loose hand-painted watercolor illustration of the subject: visible cold-press paper grain, wet-on-wet blooms and pigment granulation, a few confident ink line accents, generous white space, a restrained palette of three or four harmonious colors.",
    },
    MediaPreset {
        name: "画·线稿",
        desc: "可打印的黑白线稿涂色页",
        prompt: "A clean black-and-white line art coloring page of the subject: uniform bold outlines, no shading, no gray fills, no hatching, pure white background, simple readable shapes with enough open area to color in, printable on A4.",
    },
    MediaPreset {
        name: "画·贴纸",
        desc: "白边模切贴纸，可做表情",
        prompt: "A die-cut sticker of the subject: bold clean vector-style shapes, flat vivid colors, a thick white border following the silhouette, subtle drop shadow, glossy vinyl finish, centered on a plain neutral background, ready for a sticker pack.",
    },
    MediaPreset {
        name: "画·表情",
        desc: "九宫格表情包，同一角色九种情绪",
        prompt: "A 3x3 grid of chibi reaction stickers of one and the same character, one clear emotion per cell: happy, angry, crying, smug, shocked, sleepy, confused, adoring, thumbs-up. Thick outlines, flat bright colors, white background, consistent character design across all nine cells, a short Chinese caption of two to four characters under each face, spelled correctly.",
    },
    MediaPreset {
        name: "画·海报",
        desc: "极简排版海报，大字留白",
        prompt: "A minimal Swiss-style poster of the subject: strong typographic hierarchy, one huge headline, generous negative space, a two-color palette plus paper white, strict grid alignment, subtle print texture. Keep every word short and spelled exactly as given.",
    },
    MediaPreset {
        name: "画·图解",
        desc: "手绘知识信息图，讲清一件事",
        prompt: "A hand-drawn infographic that explains the subject: a clear title, three to five labeled sections with simple icons and arrows showing the order, marker-on-notebook aesthetic, one accent color on warm paper, neat handwriting-style labels, every word legible and correctly spelled.",
    },
    MediaPreset {
        name: "画·产品",
        desc: "电商产品主图",
        prompt: "A commercial product photograph of the subject: floating slightly above a clean seamless backdrop with a soft gradient, three-point studio lighting, crisp reflection on a subtle glossy floor, dust-free surfaces, shallow depth of field, generous empty space for copy, e-commerce hero shot.",
    },
    MediaPreset {
        name: "画·上色",
        desc: "老照片修复上色（配一张旧照）",
        prompt: "Restore and colorize the attached old photograph: repair scratches, tears, folds and stains, recover facial detail without changing the faces, natural period-accurate colors, believable skin tones, keep the original grain, framing and expressions, no modern retouching gloss, invent nothing that is not in the photo.",
    },
    MediaPreset {
        name: "画·赛博",
        desc: "赛博朋克霓虹夜景",
        prompt: "A cyberpunk night scene featuring the subject: rain-slick streets, dense neon signage, volumetric fog, strong magenta and cyan rim light, reflections in every puddle and window, 35mm lens, gritty high-contrast grade.",
    },
    MediaPreset {
        name: "画·浮世绘",
        desc: "浮世绘木版画",
        prompt: "A ukiyo-e woodblock print of the subject: flat color areas, bold sumi ink outlines, visible woodgrain and washi paper texture, a Prussian blue and vermilion palette, stylized waves and clouds, vertical composition with a decorative cartouche in one corner.",
    },
    MediaPreset {
        name: "画·云朵",
        desc: "云彩拼出主体的形状",
        prompt: "A photograph of a clear daytime sky in which the scattered clouds have arranged themselves into the unmistakable silhouette of the subject, floating high above a simple landscape or quiet rooftops. Bright natural light, realistic soft-edged cloud texture, the rest of the sky left open and clean so the shape reads at a glance.",
    },
    MediaPreset {
        name: "画·充气",
        desc: "充气玩具：软乎乎的气球感",
        prompt: "A high-resolution 3D render of the subject as an inflatable, puffy object: soft rounded air-filled volumes, smooth matte vinyl with subtle fabric creases and stitching seams, a slightly squishy irregular silhouette, gentle soft-box shadows. Floating on a clean minimal light-gray background, studio product lighting, tactile and playful.",
    },
    MediaPreset {
        name: "画·随拍",
        desc: "刻意平庸的随手自拍",
        prompt: "An extremely ordinary phone selfie of the subject, with no deliberate composition: slight motion blur, uneven indoor or late-afternoon light blowing out the highlights a little, an awkward angle, a cluttered real-life background, one arm half out of frame as if the phone was pulled from a pocket a second too late. Deliberately mundane, unedited snapshot look, no retouching.",
    },
    MediaPreset {
        name: "画·传送门",
        desc: "Q 版角色穿传送门来牵你",
        prompt: "The subject appears as a 3D chibi figure stepping out of a glowing oval portal and reaching back to pull the viewer in by the hand, glancing over its shoulder mid-motion. Inside the portal is the subject's own stylized chibi world; outside is an ordinary real-world room. Shimmering blue and purple portal light, cinematic third-person camera, the viewer's hand just visible at the frame edge.",
    },
    MediaPreset {
        name: "画·拍立得",
        desc: "从拍立得相纸里走出来",
        prompt: "The subject rendered as a 3D chibi figure printed on a Polaroid photo that a hand holds up; the figure is stepping out of the picture, breaking through the flat photo border into the space in front of it, one foot and one hand already outside the white frame. Soft daylight, clean background, believable paper texture and a thick white Polaroid margin.",
    },
    MediaPreset {
        name: "画·水晶球",
        desc: "窗边水晶球里的小世界",
        prompt: "A glass snow globe on a wooden table beside a window, containing a tiny detailed chibi scene of the subject. Warm afternoon sun shines through the glass and scatters small golden highlights, the room behind is blurred and dim. Inside the globe the little figures are lively and precise; the glass shows believable refraction and a soft rim of light.",
    },
    MediaPreset {
        name: "画·低多边形",
        desc: "低多边形三角面渲染",
        prompt: "A low-poly 3D render of the subject built from clean triangular facets, flat shaded with a restrained palette of two or three colors, standing in a stylized geometric desert of simple shapes. Crisp ambient occlusion, no textures, sharp readable silhouettes, a calm modern digital-art render.",
    },
    MediaPreset {
        name: "画·迷你城",
        desc: "微缩城市一角的可爱建筑",
        prompt: "A 3D chibi miniature diorama of a tiny two-story building shaped like an oversized everyday object related to the subject, with big windows showing a warm detailed interior of wood, lamps and tiny busy figures. A charming city corner around it: benches, street lamps, potted plants. Tilt-shift miniature photography, soft afternoon light, rich small-scale detail.",
    },
    MediaPreset {
        name: "画·壁画",
        desc: "国风城墙壁画与倾泻的花",
        prompt: "A tall traditional Chinese city wall carrying a mural of the subject in flowing hanfu; a great flowering tree leans over the wall and its blossom-heavy branches spill down, the flowers gathering into the subject's hair like a crown. Blue blossoms against pale stone, bright sky, fallen petals on the asphalt road below with a few passers-by. Ultra-detailed photorealistic mural photography.",
    },
    MediaPreset {
        name: "画·吉卜力",
        desc: "吉卜力手绘动画风",
        prompt: "Redraw the subject as a hand-painted Studio Ghibli style animation cel: soft watercolor backgrounds, layered gouache skies, gentle warm light, rounded natural shapes, expressive but simple faces, rich greens and towering cumulus clouds, a quiet sense of wonder. Traditional 2D cel animation look, visible brushwork, no 3D rendering.",
    },
    MediaPreset {
        name: "画·钥匙扣",
        desc: "Q 版橡胶钥匙扣特写",
        prompt: "A close-up photograph of a hand holding a cute colorful rubber keychain charm of the subject: chibi proportions, a bold black outline around the silhouette, soft matte rubber surface, a small silver key ring attached, gently catching the light. Neutral blurred background, shallow depth of field, crisp product-photo clarity.",
    },
    MediaPreset {
        name: "画·盲盒",
        desc: "盲盒公仔与包装盒",
        prompt: "A blind-box designer toy of the subject: a chibi vinyl figure with a matte finish and a few glossy accents, standing in front of its own illustrated window box with bold lettering and a small series logo. Clean studio lighting, soft pastel gradient backdrop, product photography, the box art matching the figure exactly.",
    },
    MediaPreset {
        name: "画·RPG卡",
        desc: "RPG 角色属性卡",
        prompt: "A collectible RPG character card of the subject: the character stands confidently with tools or symbols of its trade, rendered in a soft-lit 3D cartoon style. The card shows skill bars and stat values along one side, a title banner across the top and a name plate at the bottom, framed with clean lines like real model-kit packaging, the background keyed to the subject's theme.",
    },
    MediaPreset {
        name: "画·四格",
        desc: "四格漫画，带点幽默",
        prompt: "A colorful four-panel manga page that explains or jokes about the subject, with clear panel borders, expressive characters, simple speech bubbles and a punchline in the last panel. Clean inking, flat screentone shading, readable visual storytelling, a little humor, every caption short and correctly spelled.",
    },
    MediaPreset {
        name: "画·简笔",
        desc: "手绘简笔画表情系列",
        prompt: "Turn the subject into a hand-drawn stick-figure doodle, then show a row of six reaction faces of it: tongue out, smiling, frowning, surprised, thinking, winking. Loose black marker lines on white paper, minimal color, charmingly crude and expressive.",
    },
    MediaPreset {
        name: "画·复古广告",
        desc: "红黄放射的复古促销海报",
        prompt: "A retro promotional poster about the subject: bold Chinese headline lettering, a red-and-yellow radiating sunburst background, a polished vintage-style illustration of the subject center stage, ribbon banners and starburst price tags, halftone print texture, the look of a 1980s Chinese advertising poster. Keep the lettering short and spelled correctly.",
    },
    MediaPreset {
        name: "画·CCD",
        desc: "CCD 老数码相机的自拍",
        prompt: "A selfie of the subject shot on an old CCD compact camera: slight overexposure, harsh direct on-camera flash, washed-out colors, mild digital noise, a soft glow around the highlights and flash-lit skin, dated casual framing. Straight-out-of-camera early-2000s digital look, no modern color grading.",
    },
];

/// 选一个真正在售的模型；列表还没拉回来（或列表里没有这一族）就用兜底 id。
pub(crate) fn resolve_model(models: &[String], keyword: &str, fallback: &str) -> String {
    models
        .iter()
        .find(|model| model.to_lowercase().contains(keyword))
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// 建出还没建过的预设房间，返回新建了几间。
///
/// 判重看两处：`seeded_presets`（建过一次就不再建，删掉的不会复活）和现有房间名
/// （管理员自己占了这个名字时不覆盖）。已存在的预设房间一个字都不动——提示词是
/// 用来改的，改完不该被下次启动改回去。
pub(crate) fn seed(config: &mut Config) -> usize {
    let image_model = resolve_model(&config.models, MODEL_KEYWORD, FALLBACK_MODEL);
    let mut created = 0;
    for preset in PRESETS {
        let room = build(config, preset.name, preset.desc, &format!("{}{BRIDGE}", preset.prompt));
        if let Some(mut room) = room {
            room.model = image_model.clone();
            room.set_engine(ENGINE_CHAT, &image_model);
            room.section = SECTION.to_string();
            config.agents.push(room);
            created += 1;
        }
    }
    // 音乐与视频走的是和画图同一套「系统提示词当风格前缀」的写法，只是模型落在
    // 另外两族 id 上。各自的预设清单分开维护，增删互不影响。
    let music_model = resolve_model(
        &config.models,
        super::music::DEFAULT_MUSIC_MODELS.first().copied().unwrap_or("suno"),
        super::music::FALLBACK_MODEL,
    );
    let video_model = resolve_model(
        &config.models,
        super::video::PREFERRED_MODEL_KEYWORD,
        super::video::FALLBACK_MODEL,
    );
    for (presets, model, section, bridge) in [
        (
            MUSIC_PRESETS,
            music_model,
            MUSIC_SECTION,
            MUSIC_BRIDGE,
        ),
        (
            VIDEO_PRESETS,
            video_model,
            VIDEO_SECTION,
            VIDEO_BRIDGE,
        ),
    ] {
        for preset in presets {
            let room = build(config, preset.name, preset.desc, &format!("{}{bridge}", preset.prompt));
            if let Some(mut room) = room {
                room.model = model.clone();
                room.set_engine(ENGINE_CHAT, &model);
                room.section = section.to_string();
                config.agents.push(room);
                created += 1;
            }
        }
    }
    created
}

/// 记下这一间「建过了」，名字没被占用时返回要建的房间。管理员自己占了这个名字时
/// 只记名不覆盖——下次启动也不再重试。
fn build(config: &mut Config, name: &str, desc: &str, prompt: &str) -> Option<Agent> {
    if config
        .seeded_presets
        .iter()
        .any(|seeded| seeded.eq_ignore_ascii_case(name))
    {
        return None;
    }
    config.seeded_presets.push(name.to_string());
    if config
        .agents
        .iter()
        .any(|agent| agent.name.eq_ignore_ascii_case(name))
    {
        return None;
    }
    Some(Agent::new(name, "", prompt, desc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::oai::parser::valid_agent_name;

    /// 名字得能被房间指令认出来，又不能被日常聊天误触发。
    #[test]
    fn preset_names_are_addressable_but_hard_to_trigger_by_accident() {
        let mut seen = std::collections::HashSet::new();
        for (presets, prefix) in [
            (PRESETS, PREFIX),
            (MUSIC_PRESETS, MUSIC_PREFIX),
            (VIDEO_PRESETS, VIDEO_PREFIX),
        ] {
            for preset in presets {
                assert!(
                    valid_agent_name(preset.name),
                    "{} 不是合法房间名",
                    preset.name
                );
                assert!(preset.name.starts_with(prefix), "{}", preset.name);
                assert!(
                    seen.insert(preset.name.to_lowercase()),
                    "{} 重名",
                    preset.name
                );
                assert!(!preset.desc.is_empty() && !preset.prompt.is_empty());
                // 描述在列表里只显示 20 个字，超了就看不出这间房是干什么的。
                assert!(preset.desc.chars().count() <= 20, "{}", preset.desc);
                // 前缀后面一定还有字：光一个前缀既不像房间名，也拦不住误触发。
                assert!(preset.name.chars().count() > 2, "{}", preset.name);
            }
        }
    }

    /// 建过一次就不再建；管理员删掉的房间不会在下次启动时复活。
    #[test]
    fn seeding_is_idempotent_and_deletions_stick() {
        let mut config = Config {
            models: vec!["gpt-4o".into(), "gpt-image-2.5-flare".into()],
            ..Default::default()
        };
        let total = PRESETS.len() + MUSIC_PRESETS.len() + VIDEO_PRESETS.len();
        assert_eq!(seed(&mut config), total);
        assert_eq!(config.agents.len(), total);
        let room = config.agents.iter().find(|a| a.name == "画·手办").unwrap();
        assert_eq!(room.model, "gpt-image-2.5-flare");
        assert_eq!(room.section, SECTION);
        assert!(room.system_prompt.contains("1/7 scale"));
        assert!(room.system_prompt.ends_with("以下是这次要画的东西："));

        // 再跑一次什么都不建。
        assert_eq!(seed(&mut config), 0);
        // 删掉一间，下次启动也不再建。
        config.agents.retain(|agent| agent.name != "画·手办");
        assert_eq!(seed(&mut config), 0);
        assert!(!config.agents.iter().any(|a| a.name == "画·手办"));

        // 新增预设只补建没见过的那一间。
        config.seeded_presets.retain(|name| name != "画·乐高");
        config.agents.retain(|agent| agent.name != "画·乐高");
        assert_eq!(seed(&mut config), 1);
    }

    /// 同名房间已被占用时不覆盖，但也不再反复尝试。
    #[test]
    fn an_existing_room_of_the_same_name_is_left_alone() {
        let mut config = Config::default();
        let mut mine = Agent::new("画·手办", "gpt-4o", "我自己的提示词", "我的房间");
        mine.set_engine(ENGINE_CHAT, "gpt-4o");
        config.agents.push(mine);
        seed(&mut config);
        let room = config.agents.iter().find(|a| a.name == "画·手办").unwrap();
        assert_eq!(room.system_prompt, "我自己的提示词");
        assert!(room.section.is_empty());
        assert_eq!(config.agents.iter().filter(|a| a.name == "画·手办").count(), 1);
    }

    /// 建出来的房间必须真的走各自的专用接口，否则预设只是一段被聊天模型读掉的废话。
    #[test]
    fn preset_rooms_route_to_their_own_endpoint() {
        let keywords = |list: &[&str]| -> Vec<String> {
            list.iter().map(|keyword| (*keyword).to_string()).collect()
        };
        let images = keywords(super::super::images::DEFAULT_IMAGE_MODELS);
        let music = keywords(super::super::music::DEFAULT_MUSIC_MODELS);
        let video = keywords(super::super::video::DEFAULT_VIDEO_MODELS);

        let mut config = Config::default();
        seed(&mut config);
        for room in &config.agents {
            let matches = if room.name.starts_with(MUSIC_PREFIX) {
                super::super::music::is_music_model(&room.model, &music)
            } else if room.name.starts_with(VIDEO_PREFIX) {
                super::super::video::is_video_model(&room.model, &video)
            } else {
                super::super::images::is_images_model(&room.model, &images)
            };
            assert!(matches, "{} 的模型 {} 落不到专用接口", room.name, room.model);
            assert!(!room.uses_pi(), "{}", room.name);
            // 三族模型关键字必须互不误伤：一间房只能命中一个专用接口。
            assert!(
                !(super::super::images::is_images_model(&room.model, &images)
                    && super::super::video::is_video_model(&room.model, &video)),
                "{} 的模型同时命中画图与视频关键字",
                room.name
            );
        }
    }

    /// 模型列表还没拉回来时也建得出来，拉回来之后按实际在售的 id 建。
    #[test]
    fn the_model_comes_from_whatever_the_site_actually_sells() {
        assert_eq!(resolve_model(&[], MODEL_KEYWORD, FALLBACK_MODEL), FALLBACK_MODEL);
        assert_eq!(
            resolve_model(
                &["gpt-4o".into(), "gpt-image-2.5-sunburst".into()],
                MODEL_KEYWORD,
                FALLBACK_MODEL
            ),
            "gpt-image-2.5-sunburst"
        );
        // 音乐与视频走同一套挑选逻辑，只是关键字与兜底 id 不同。
        assert_eq!(
            resolve_model(&[], "suno", super::super::music::FALLBACK_MODEL),
            "suno_music"
        );
        // 预设在同族里挑 fast：pro 贵好几倍，不该被自动选中。
        assert_eq!(
            resolve_model(
                &["veo3.1-pro".into(), "veo3.1-fast".into()],
                super::super::video::PREFERRED_MODEL_KEYWORD,
                super::super::video::FALLBACK_MODEL
            ),
            "veo3.1-fast"
        );
    }
}
