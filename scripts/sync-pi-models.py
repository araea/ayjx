#!/usr/bin/env python3
"""把中转站当前在售的模型登记进本机 pi 的 `~/.pi/agent/models.json`。

Pi 房间的模型就是 pi 的 `--model`。未登记的 id 也能发出去，但 pi 只能对它套用
默认的上下文窗口、输出上限和「纯文本」模态——于是发图的房间悄悄丢掉图片，长对话
在真正超限之前就被压缩。登记一次，这些账就都算得准了。

模型名单沿用 ayjx `[oai].model_filter`：`/%` 看到的和 Pi 房间能选的是同一批，
中转站上新或下架时只改那一处，然后重跑本脚本。

    python3 scripts/sync-pi-models.py            # 写入
    python3 scripts/sync-pi-models.py --dry-run  # 只看要写什么
    python3 scripts/sync-pi-models.py --probe    # 逐个实测识图能力再写

中转站自己的 `tags` 会漏标（实测里 Claude Sonnet 5 被标成纯文本），所以 `--probe`
给每个模型发一张真图看它认不认。默认不测，沿用 tags；两者取并集，只多不少。

不改 `settings.json`，也不碰 pi 的其它配置。
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OAI_CONFIG = REPO / "target" / "release" / "data" / "oai" / "config.json"
BOT_CONFIG = REPO / "config.toml"
MODELS_JSON = Path.home() / ".pi" / "agent" / "models.json"
PROVIDER = "apilio"

# 中转站不报告上下文窗口，这里按模型族给一份保守值：宁可让 pi 早一点压缩，
# 也不要让它把超限的请求发出去。只影响 pi 这边的记账，不改变实际请求。
FAMILIES: list[tuple[tuple[str, ...], int, int]] = [
    (("gemini-3.",), 1_048_576, 65_536),
    (("gpt-5.5",), 400_000, 128_000),
    (("gpt-5.6",), 272_000, 32_768),
    (("deepseek-v4",), 393_216, 65_536),
    (("claude-opus-5", "claude-sonnet-5", "claude-fable-5"), 200_000, 64_000),
    (("claude-opus-4",), 200_000, 32_000),
    (("grok-4.",), 256_000, 32_768),
    (("kimi-k",), 256_000, 32_768),
    (("qwen3.",), 262_144, 32_768),
    (("glm-5.", "minimax-m2.", "mimo-v2."), 204_800, 32_768),
]
DEFAULT_WINDOW = (128_000, 16_384)

# 中转站按倍率计费：1 倍率 = $2 / 1M token（new-api 的通用换算）。
RATIO_TO_USD_PER_MTOK = 2.0


def die(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_filter() -> tuple[list[str], list[str]]:
    """读 ayjx 的 `[oai].model_filter`，拿到 keep / drop 关键字。"""
    try:
        import tomllib
    except ModuleNotFoundError:  # Python < 3.11
        die("需要 Python 3.11+ 的 tomllib")
    if not BOT_CONFIG.exists():
        die(f"找不到 {BOT_CONFIG}")
    config = tomllib.loads(BOT_CONFIG.read_text(encoding="utf-8"))
    model_filter = config.get("oai", {}).get("model_filter", {})
    keep = [k.lower() for k in model_filter.get("keep", [])]
    drop = [d.lower() for d in model_filter.get("drop", [])]
    if not keep:
        die("[oai.model_filter].keep 是空的，先在 config.toml 里挑出要用的模型")
    return keep, drop


def matches(model: str, pattern: str) -> bool:
    """与 ayjx 的过滤一致：`*` 结尾是前缀匹配，其余按子串匹配。"""
    return model.startswith(pattern[:-1]) if pattern.endswith("*") else pattern in model


def relay() -> tuple[str, str]:
    if not OAI_CONFIG.exists():
        die(f"找不到 {OAI_CONFIG}；先让 ayjx 至少启动过一次")
    config = json.loads(OAI_CONFIG.read_text(encoding="utf-8"))
    base, key = config.get("api_base", ""), config.get("api_key", "")
    if not base or not key:
        die("ayjx 的 oai 配置里还没有 api_base / api_key")
    return base.rstrip("/"), key


def fetch(url: str, key: str) -> dict:
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {key}"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.loads(response.read().decode("utf-8"))


def window(model: str) -> tuple[int, int]:
    lowered = model.lower()
    for prefixes, context, output in FAMILIES:
        if any(prefix in lowered for prefix in prefixes):
            return context, output
    return DEFAULT_WINDOW


def cost(entry: dict) -> dict | None:
    """把中转站的倍率换算成 pi 记账用的 $/1M token。"""
    ratio = entry.get("model_ratio")
    if not isinstance(ratio, (int, float)) or ratio <= 0:
        return None
    price_in = round(ratio * RATIO_TO_USD_PER_MTOK, 4)
    completion = entry.get("completion_ratio") or 1
    cache = entry.get("cache_ratio") or 0
    return {
        "input": price_in,
        "output": round(price_in * completion, 4),
        "cacheRead": round(price_in * cache, 4),
        "cacheWrite": round(price_in * (entry.get("create_cache_ratio") or 0), 4),
    }


# 一张 256x256 的纯色 PNG。再小就会被好几家上游当成损坏的图片退回来，
# 那时候的报错和「不支持图片」长得一模一样，分不出来。
PROBE_PNG = (
    "iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAIAAADTED8xAAAACXBIWXMAAAABAAAAAQBPJcTWAAAC"
    "vUlEQVR4nO3TMQ0AMAzAsE0af8jVYPSIjSBP7hzoetsBsMkApBmANAOQZgDSDECaAUgzAGkGIM0A"
    "pBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgz"
    "AGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDS"
    "DECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmA"
    "NAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkG"
    "IM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECa"
    "AUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQ"
    "ZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0A"
    "pBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgz"
    "AGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDS"
    "DECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmA"
    "NAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkGIM0ApBmANAOQZgDSDECaAUgzAGkG"
    "IM0ApBmANAOQZgDSDECaAUj7g9UE/Hc1F54AAAAASUVORK5CYII="
)


def sees_images(model: str, base: str, key: str) -> bool | None:
    """真发一张图，看这个模型认不认。`None` 表示没测出结论。"""
    body = json.dumps(
        {
            "model": model,
            "max_tokens": 16,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "图里主要是什么颜色？只回一个词"},
                        {
                            "type": "image_url",
                            "image_url": {"url": f"data:image/png;base64,{PROBE_PNG}"},
                        },
                    ],
                }
            ],
        }
    ).encode()
    request = urllib.request.Request(
        f"{base}/chat/completions",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=180):
            return True
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace").lower()
        # 只有「这个模型不收图片」才是结论。额度、风控、上游过载、404 都不是
        # ——实测里 claude-fable-5-1 是 upstream overloaded、kimi-k2.6 是账号欠费，
        # 把它们当成纯文本会白白关掉两个能识图的模型。
        refused = ("allowed values" in detail and "text" in detail) or any(
            phrase in detail
            for phrase in (
                "vision is disabled",
                "does not support image",
                "not support vision",
                "text only",
            )
        )
        return False if refused else None
    except Exception:
        return None


def probe_all(models: list[str], base: str, key: str) -> dict[str, bool | None]:
    with concurrent.futures.ThreadPoolExecutor(6) as pool:
        probed = dict(zip(models, pool.map(lambda m: sees_images(m, base, key), models)))
    # 一次实测只是一次网络请求：上游过载、账号欠费都会让结论变成 None。
    # `-thinking` 只是同一个模型的推理档，模态跟着基座走，别让一次抖动把它降级成纯文本。
    for model, seen in probed.items():
        if seen is not None:
            continue
        base_id = model.split("-thinking", 1)[0]
        if base_id != model and probed.get(base_id) is not None:
            probed[model] = probed[base_id]
    return probed


def build(
    models: list[str], pricing: dict[str, dict], probed: dict[str, bool | None]
) -> list[dict]:
    out = []
    for model in models:
        entry = pricing.get(model, {})
        tags = entry.get("tags") or ""
        # tags 会漏标，实测会被额度和风控干扰；两者取并集，只在双方都否定时才当纯文本。
        vision = "识图" in tags or "视觉" in tags or probed.get(model) is True
        if probed.get(model) is False:
            vision = False
        context, max_tokens = window(model)
        built = {
            "id": model,
            "name": f"{model} (中转站)",
            "input": ["text", "image"] if vision else ["text"],
            "contextWindow": context,
            "maxTokens": max_tokens,
            # OpenAI 兼容端点普遍只认 max_tokens，中转站也一样。
            "compat": {"maxTokensField": "max_tokens"},
        }
        if model.endswith("-thinking") or "-thinking-" in model:
            built["reasoning"] = True
        if price := cost(entry):
            built["cost"] = price
        out.append(built)
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="只打印，不写文件")
    parser.add_argument(
        "--probe", action="store_true", help="逐个模型实测识图能力，而不是只信 tags"
    )
    args = parser.parse_args()

    keep, drop = load_filter()
    base, key = relay()
    listed = [m["id"] for m in fetch(f"{base}/models", key).get("data", [])]
    pricing = {
        m["model_name"]: m
        for m in fetch(f"{base.rsplit('/v1', 1)[0]}/api/pricing", key).get("data", [])
        if m.get("model_name")
    }

    chosen = sorted(
        model
        for model in listed
        if any(matches(model.lower(), k) for k in keep)
        and not any(matches(model.lower(), d) for d in drop)
        # 绘图接口不是对话模型，pi 用不上。
        and not any(word in model.lower() for word in ("image", "seedream", "mj"))
        # 中转站的模型表里混着 `...-thinking-*` 这样的通配占位，不是可调用的 id。
        and "*" not in model
    )
    if not chosen:
        die("过滤之后一个模型都不剩，检查 [oai.model_filter]")

    if not MODELS_JSON.exists():
        die(f"找不到 {MODELS_JSON}；先配置好 pi 的 provider 和密钥")
    config = json.loads(MODELS_JSON.read_text(encoding="utf-8"))
    provider = config.get("providers", {}).get(PROVIDER)
    if provider is None:
        die(f"pi 里还没有名为 {PROVIDER} 的 provider，先手动建好并填上密钥")

    probed: dict[str, bool | None] = {}
    if args.probe:
        print(f"实测 {len(chosen)} 个模型的识图能力，需要几分钟…")
        probed = probe_all(chosen, base, key)
        unknown = [m for m, v in probed.items() if v is None]
        if unknown:
            print(f"  {len(unknown)} 个没测出结论，沿用 tags：{', '.join(unknown)}")

    provider["models"] = build(chosen, pricing, probed)
    rendered = json.dumps(config, ensure_ascii=False, indent=2) + "\n"

    vision = sum(1 for m in provider["models"] if "image" in m["input"])
    print(f"{PROVIDER}: {len(chosen)} 个模型（其中 {vision} 个能识图）")
    for model in provider["models"]:
        print(f"  {model['id']:<38} {'识图' if 'image' in model['input'] else '文本'}"
              f"  ctx={model['contextWindow']}")
    if args.dry_run:
        return

    backup = MODELS_JSON.with_suffix(f".json.backup-{time.strftime('%Y%m%d-%H%M%S')}")
    shutil.copy2(MODELS_JSON, backup)
    tmp = MODELS_JSON.with_suffix(".json.tmp")
    tmp.write_text(rendered, encoding="utf-8")
    os.replace(tmp, MODELS_JSON)
    print(f"\n已写入 {MODELS_JSON}（备份 {backup.name}）")


if __name__ == "__main__":
    main()
