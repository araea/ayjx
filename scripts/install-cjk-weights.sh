#!/bin/sh
# 把真中日韩粗体装进 ~/.fonts，卡片的题字与加粗才有真字重。
#
# 为什么需要这一步：Android 自带的 Noto Serif/Sans CJK 只有 Regular 一档。
# 向系统要 Bold，拿回来的还是那张 400 的脸——浏览器会自己合成伪粗体，
# ayjx 的原生绘制也会（见 painter.rs 的 `Typeface.embolden`），但两者都只是
# 把轮廓外扩一圈，笔画的粗细对比和三角字脚是补不出来的。装上真字重之后，
# 两条出图路径都会自动改用它，合成逻辑自己关掉，代码一行不用动。
#
# 装完不做任何配置：fontconfig 的 ~/.fonts 与 fontdb 的同名目录都会自动索引。
# 不装也能跑，只是题字看着虚一档。
#
#     sh scripts/install-cjk-weights.sh          # 装
#     sh scripts/install-cjk-weights.sh --check  # 只看装没装
#
# 核对本机实际拿到的字重：
#     cargo test ciyi::painter::tests::report -- --ignored --nocapture

set -eu

DEST="${FONT_DIR:-$HOME/.fonts}"
BASE=https://github.com/notofonts/noto-cjk/raw/main

# 题字用 Black(900)，正文加粗用 Bold(700)；黑体只需要 Bold——
# 它只管数字与元信息，那些地方不出题字。
FILES="
Serif/OTF/SimplifiedChinese/NotoSerifCJKsc-Black.otf
Serif/OTF/SimplifiedChinese/NotoSerifCJKsc-Bold.otf
Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf
"

check() {
    missing=0
    for path in $FILES; do
        name=$(basename "$path")
        if [ -s "$DEST/$name" ]; then
            printf '  有  %s (%s)\n' "$name" "$(du -h "$DEST/$name" | cut -f1)"
        else
            printf '  缺  %s\n' "$name"
            missing=1
        fi
    done
    return $missing
}

if [ "${1:-}" = "--check" ]; then
    printf '%s\n' "$DEST:"
    check || { printf '未装齐，跑 sh scripts/install-cjk-weights.sh 补上。\n'; exit 1; }
    exit 0
fi

command -v curl >/dev/null || { printf '需要 curl。\n' >&2; exit 1; }
mkdir -p "$DEST"

for path in $FILES; do
    name=$(basename "$path")
    if [ -s "$DEST/$name" ]; then
        printf '已有 %s，跳过。\n' "$name"
        continue
    fi
    printf '下载 %s …\n' "$name"
    # 先落到临时文件，中途断网不会留下一个半截的字体让 fontconfig 索引
    if ! curl -fsSL -o "$DEST/$name.part" "$BASE/$path"; then
        rm -f "$DEST/$name.part"
        printf '下载失败：%s\n' "$name" >&2
        exit 1
    fi
    # OpenType/CFF 的魔数是 'OTTO'；拿到一页 HTML 错误提示时就是它对不上
    if [ "$(head -c4 "$DEST/$name.part" | od -An -tx1 | tr -d ' \n')" != "4f54544f" ]; then
        rm -f "$DEST/$name.part"
        printf '下载到的不是 OTF：%s\n' "$name" >&2
        exit 1
    fi
    mv "$DEST/$name.part" "$DEST/$name"
done

command -v fc-cache >/dev/null && fc-cache -f "$DEST" >/dev/null 2>&1 || true
printf '\n装好了：\n'
check
printf '\n出图进程需要重启才会重新加载字体（字体在进程内只加载一次）。\n'
