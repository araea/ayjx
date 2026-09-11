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

/// 接在每份风格提示词后面的一句，把用户那半句话接上。
///
/// 用户可能什么都不写只发一张图（「画·手办」+ 一张照片），也可能只写一句描述，
/// 所以这句话两边都要兜住，且不能反过来盖掉风格。
const BRIDGE: &str = "\nIf reference images are provided, keep the subject in them recognizable and apply the style above to it; otherwise create the subject described below. Follow any extra instruction in the user's text, but keep the style above.\n以下是这次要画的东西：";

/// 一间预设房间。
pub(crate) struct Preset {
    /// 房间名，含 [`PREFIX`]。
    pub name: &'static str,
    /// 列表里那一行说明。
    pub desc: &'static str,
    /// 风格提示词，[`BRIDGE`] 会接在它后面。
    pub prompt: &'static str,
}

/// 预设清单。提示词取自公开分享的绘图 prompt，改写成「风格前缀 + 用户描述」的形状。
pub(crate) const PRESETS: &[Preset] = &[
    Preset {
        name: "画·手办",
        desc: "手办化：1/7 比例 PVC 模型摆拍",
        prompt: "Turn the subject into a high-quality 1/7 scale collectible PVC figure standing on a round transparent acrylic base, photographed on a tidy modern desk. Behind it sits the printed retail box showing the same character as key art, and a computer monitor displaying the ZBrush sculpting process of that figure. Realistic plastic and fabric materials, soft studio lighting, shallow depth of field, product photography.",
    },
    Preset {
        name: "画·毛绒",
        desc: "毛绒玩偶化：布料质感与针脚",
        prompt: "Turn the subject into an adorable handmade plush toy: chunky rounded proportions, visible fuzzy fabric texture, neat stitching seams, embroidered eyes and nose, a small cloth tag on the side. Sitting on a soft bed in warm window light, shallow depth of field, cozy lifestyle photo.",
    },
    Preset {
        name: "画·乐高",
        desc: "乐高小人仔与盒装套组",
        prompt: "Turn the subject into a LEGO minifigure with brick-built accessories that match its character, standing on a LEGO baseplate in front of its own boxed set; the retail box shows the character artwork and a collector badge. Glossy ABS plastic, crisp studio product photography, bright even lighting, clean background.",
    },
    Preset {
        name: "画·黏土",
        desc: "黏土定格动画风，手作痕迹",
        prompt: "Render the subject as a handmade polymer clay stop-motion character: visible fingerprints and sculpting tool marks, slightly imperfect shapes, felt and cardboard set pieces around it, tilt-shift miniature feel, warm practical lighting, the charm of a frame from a stop-motion short film.",
    },
    Preset {
        name: "画·像素",
        desc: "16 位像素风游戏画面",
        prompt: "Render the subject as 16-bit era pixel art: a limited 32-color palette, crisp readable silhouette, dithered shading, dark outlines, no anti-aliasing, placed in a side-scrolling game scene with a parallax background and a small HUD in the corner.",
    },
    Preset {
        name: "画·微缩",
        desc: "等距微缩场景，玻璃罩里的小世界",
        prompt: "Render the subject as an isometric miniature diorama inside a glass display cube: tiny highly detailed props and figures, tilt-shift miniature effect, soft volumetric light, 45-degree isometric camera, clean neutral background, crisp 3D render.",
    },
    Preset {
        name: "画·剧照",
        desc: "电影剧照质感的人像",
        prompt: "A cinematic film still of the subject: 85mm lens, shallow depth of field, motivated practical lighting with a strong key and soft fill, subtle film grain, teal and amber color grade, gentle anamorphic flare, 2.39:1 framing, composed like a frame from a prestige drama.",
    },
    Preset {
        name: "画·证件",
        desc: "正装职业头像 / 证件照",
        prompt: "A professional headshot of the subject: neutral light-gray seamless backdrop, business attire, large soft key light with gentle fill, clear catchlights in the eyes, natural skin texture preserved, sharp focus on the eyes, 85mm f/2.8 look, suitable for a résumé or a company profile page. Keep the person's facial identity and features unchanged.",
    },
    Preset {
        name: "画·水彩",
        desc: "手绘水彩插画，留白干净",
        prompt: "A loose hand-painted watercolor illustration of the subject: visible cold-press paper grain, wet-on-wet blooms and pigment granulation, a few confident ink line accents, generous white space, a restrained palette of three or four harmonious colors.",
    },
    Preset {
        name: "画·线稿",
        desc: "可打印的黑白线稿涂色页",
        prompt: "A clean black-and-white line art coloring page of the subject: uniform bold outlines, no shading, no gray fills, no hatching, pure white background, simple readable shapes with enough open area to color in, printable on A4.",
    },
    Preset {
        name: "画·贴纸",
        desc: "白边模切贴纸，可做表情",
        prompt: "A die-cut sticker of the subject: bold clean vector-style shapes, flat vivid colors, a thick white border following the silhouette, subtle drop shadow, glossy vinyl finish, centered on a plain neutral background, ready for a sticker pack.",
    },
    Preset {
        name: "画·表情",
        desc: "九宫格表情包，同一角色九种情绪",
        prompt: "A 3x3 grid of chibi reaction stickers of one and the same character, one clear emotion per cell: happy, angry, crying, smug, shocked, sleepy, confused, adoring, thumbs-up. Thick outlines, flat bright colors, white background, consistent character design across all nine cells, a short Chinese caption of two to four characters under each face, spelled correctly.",
    },
    Preset {
        name: "画·海报",
        desc: "极简排版海报，大字留白",
        prompt: "A minimal Swiss-style poster of the subject: strong typographic hierarchy, one huge headline, generous negative space, a two-color palette plus paper white, strict grid alignment, subtle print texture. Keep every word short and spelled exactly as given.",
    },
    Preset {
        name: "画·图解",
        desc: "手绘知识信息图，讲清一件事",
        prompt: "A hand-drawn infographic that explains the subject: a clear title, three to five labeled sections with simple icons and arrows showing the order, marker-on-notebook aesthetic, one accent color on warm paper, neat handwriting-style labels, every word legible and correctly spelled.",
    },
    Preset {
        name: "画·产品",
        desc: "电商产品主图",
        prompt: "A commercial product photograph of the subject: floating slightly above a clean seamless backdrop with a soft gradient, three-point studio lighting, crisp reflection on a subtle glossy floor, dust-free surfaces, shallow depth of field, generous empty space for copy, e-commerce hero shot.",
    },
    Preset {
        name: "画·上色",
        desc: "老照片修复上色（配一张旧照）",
        prompt: "Restore and colorize the attached old photograph: repair scratches, tears, folds and stains, recover facial detail without changing the faces, natural period-accurate colors, believable skin tones, keep the original grain, framing and expressions, no modern retouching gloss, invent nothing that is not in the photo.",
    },
    Preset {
        name: "画·赛博",
        desc: "赛博朋克霓虹夜景",
        prompt: "A cyberpunk night scene featuring the subject: rain-slick streets, dense neon signage, volumetric fog, strong magenta and cyan rim light, reflections in every puddle and window, 35mm lens, gritty high-contrast grade.",
    },
    Preset {
        name: "画·浮世绘",
        desc: "浮世绘木版画",
        prompt: "A ukiyo-e woodblock print of the subject: flat color areas, bold sumi ink outlines, visible woodgrain and washi paper texture, a Prussian blue and vermilion palette, stylized waves and clouds, vertical composition with a decorative cartouche in one corner.",
    },
];

/// 选一个真正在售的图像模型；列表还没拉回来就用兜底 id。
pub(crate) fn resolve_model(models: &[String]) -> String {
    models
        .iter()
        .find(|model| model.to_lowercase().contains(MODEL_KEYWORD))
        .cloned()
        .unwrap_or_else(|| FALLBACK_MODEL.to_string())
}

/// 建出还没建过的预设房间，返回新建了几间。
///
/// 判重看两处：`seeded_presets`（建过一次就不再建，删掉的不会复活）和现有房间名
/// （管理员自己占了这个名字时不覆盖）。已存在的预设房间一个字都不动——提示词是
/// 用来改的，改完不该被下次启动改回去。
pub(crate) fn seed(config: &mut Config) -> usize {
    let model = resolve_model(&config.models);
    let mut created = 0;
    for preset in PRESETS {
        if config
            .seeded_presets
            .iter()
            .any(|name| name.eq_ignore_ascii_case(preset.name))
        {
            continue;
        }
        config.seeded_presets.push(preset.name.to_string());
        if config
            .agents
            .iter()
            .any(|agent| agent.name.eq_ignore_ascii_case(preset.name))
        {
            continue;
        }
        let mut room = Agent::new(
            preset.name,
            &model,
            &format!("{}{BRIDGE}", preset.prompt),
            preset.desc,
        );
        room.set_engine(ENGINE_CHAT, &model);
        room.section = SECTION.to_string();
        config.agents.push(room);
        created += 1;
    }
    created
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::oai::parser::valid_agent_name;

    /// 名字得能被房间指令认出来，又不能被日常聊天误触发。
    #[test]
    fn preset_names_are_addressable_but_hard_to_trigger_by_accident() {
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(
                valid_agent_name(preset.name),
                "{} 不是合法房间名",
                preset.name
            );
            assert!(preset.name.starts_with(PREFIX), "{}", preset.name);
            assert!(seen.insert(preset.name.to_lowercase()), "{} 重名", preset.name);
            assert!(!preset.desc.is_empty() && !preset.prompt.is_empty());
            // 描述在列表里只显示 20 个字，超了就看不出这间房是干什么的。
            assert!(preset.desc.chars().count() <= 20, "{}", preset.desc);
        }
        // 前缀后面一定还有字：光一个「画·」既不像房间名，也拦不住误触发。
        assert!(PRESETS.iter().all(|preset| preset.name.chars().count() > 2));
    }

    /// 建过一次就不再建；管理员删掉的房间不会在下次启动时复活。
    #[test]
    fn seeding_is_idempotent_and_deletions_stick() {
        let mut config = Config {
            models: vec!["gpt-4o".into(), "gpt-image-2.5-flare".into()],
            ..Default::default()
        };
        assert_eq!(seed(&mut config), PRESETS.len());
        assert_eq!(config.agents.len(), PRESETS.len());
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

    /// 建出来的房间必须真的走图像接口，否则预设只是一段被聊天模型读掉的废话。
    #[test]
    fn preset_rooms_route_to_the_image_endpoint() {
        let keywords: Vec<String> = super::super::images::DEFAULT_IMAGE_MODELS
            .iter()
            .map(|keyword| (*keyword).to_string())
            .collect();
        let mut config = Config::default();
        seed(&mut config);
        for room in &config.agents {
            assert!(
                super::super::images::is_images_model(&room.model, &keywords),
                "{} 的模型 {} 不会走绘图接口",
                room.name,
                room.model
            );
            assert!(!room.uses_pi(), "{}", room.name);
        }
    }

    /// 模型列表还没拉回来时也建得出来，拉回来之后按实际在售的 id 建。
    #[test]
    fn the_model_comes_from_whatever_the_site_actually_sells() {
        assert_eq!(resolve_model(&[]), FALLBACK_MODEL);
        assert_eq!(
            resolve_model(&["gpt-4o".into(), "gpt-image-2.5-sunburst".into()]),
            "gpt-image-2.5-sunburst"
        );
    }
}
