"""Build bundled webfonts for Ruthenium (reproducible).

策略：**内嵌全量 Noto Sans CJK，但按 unicode-range 分块加载**。
- 不按语料裁剪字形：任何字符（含用户输入 / 导入的生僻字）都能渲染。
- 语料只用来决定「把哪些码位打包进同一块」——常用字放进最前面的 1–2 块，
  这样首屏只解码几百 KB；其余码位按码位顺序切成约 1200 字一批，用到时才下载。
  **没有任何码位被丢弃**：所有块 cmap 的并集 == 源字体 cmap（verify-fonts.py 会硬校验）。
- 单套 Pan-CJK 字体即可：三个地区字形由 `locl` + `html[lang]` 选择
  （Chromium/WebView2 上生效：直/骨/令 在中日台三地字形不同）。

输出：
  src/assets/fonts/noto-cjk-c00.woff2 …（UI 分块）
  src/assets/fonts/jetbrains-mono.woff2 / source-code-pro.woff2 / ibm-plex-mono.woff2
  src/styles/fonts.css                （构建期生成，勿手改：@font-face + unicode-range）
  scripts/.fontcache/fonts.manifest.json（分块清单，供校验脚本使用）

Usage:   python scripts/build-fonts.py
Needs:   fonttools + brotli   (pip install fonttools brotli)
"""
from __future__ import annotations

import json
import pathlib
import re
import urllib.request

from fontTools import subset
from fontTools.ttLib import TTFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = ROOT / "scripts" / ".fontcache"
OUT = ROOT / "src" / "assets" / "fonts"
CSS = ROOT / "src" / "styles" / "fonts.css"

GF = "https://raw.githubusercontent.com/google/fonts/main/ofl/"
CJK = "https://raw.githubusercontent.com/notofonts/noto-cjk/main/Sans/Variable/OTF/"

UI_SOURCE = "NotoSansCJKsc-VF.otf"
UI_URL = CJK + UI_SOURCE
MONO_SOURCES = {
    "jetbrains-mono": GF + "jetbrainsmono/JetBrainsMono%5Bwght%5D.ttf",
    "source-code-pro": GF + "sourcecodepro/SourceCodePro%5Bwght%5D.ttf",
    "ibm-plex-mono": GF + "ibmplexmono/IBMPlexMono-Regular.ttf",
}

# 每块的目标字形数：常用块小一点（首屏要解码），其余块大一点（减少请求数）。
COMMON_CHUNK = 900
REST_CHUNK = 1200

# 语料扫描（只用于决定分块顺序，不参与裁剪）
LITERAL = re.compile(r"""'([^'\\\n]|\\.)*'|"([^"\\\n]|\\.)*"|`([^`\\]|\\.)*`""", re.S)
COMMENTS = re.compile(r"/\*.*?\*/|//[^\n]*", re.S)


def strip_comments(text: str) -> str:
    return COMMENTS.sub(" ", text)


def corpus() -> dict[str, set[str]]:
    """字符串字面量里出现的字符：四语言词条 + 其它 UI/种子数据。"""
    per: dict[str, set[str]] = {"zh-CN": set(), "zh-TW": set(), "en": set(), "ja": set()}
    ui: set[str] = set()
    for p in (ROOT / "src").rglob("*"):
        if p.suffix not in {".ts", ".tsx"}:
            continue
        text = strip_comments(p.read_text(encoding="utf-8"))
        chars: set[str] = set()
        for m in LITERAL.finditer(text):
            chars.update(m.group(0))
        chars = {c for c in chars if c.isprintable() and c != "\\"}
        if p.parent.name == "locales" and p.stem in per:
            per[p.stem] |= chars
        else:
            ui |= chars
    return {"ui": ui, **per}


def ascii_set() -> set[str]:
    return {chr(c) for c in range(0x20, 0x7F)}


def download(name: str, url: str) -> pathlib.Path:
    CACHE.mkdir(parents=True, exist_ok=True)
    suffix = pathlib.Path(url).suffix or ".ttf"
    dest = CACHE / (name if name.endswith(suffix) else f"{name}{suffix}")
    if dest.exists() and dest.stat().st_size > 10_000:
        return dest
    print(f"  download {name} ...")
    urllib.request.urlretrieve(url, dest)
    return dest


def to_woff2(src: pathlib.Path, out: pathlib.Path) -> None:
    """整包转换（等宽字体只有几十 KB，直接全量）。"""
    font = TTFont(str(src))
    font.flavor = "woff2"
    font.save(str(out))


def write_subset(src: pathlib.Path, out: pathlib.Path, codepoints: list[int]) -> None:
    """按码位子集化并写 woff2；保留全部 layout features（locl/vert 等地区字形必需）。"""
    font = TTFont(str(src))
    opts = subset.Options()
    opts.flavor = "woff2"
    opts.layout_features = ["*"]   # 关键：保留 locl（中日台字形差异靠它）
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.notdef_outline = True
    opts.recommended_glyphs = True
    opts.hinting = False
    ss = subset.Subsetter(options=opts)
    ss.populate(unicodes=codepoints)
    ss.subset(font)
    font.flavor = "woff2"
    out.parent.mkdir(parents=True, exist_ok=True)
    font.save(str(out))
    font.close()


def to_ranges(cps: list[int]) -> list[tuple[int, int]]:
    """合并连续码位为闭区间，产出紧凑的 unicode-range。"""
    out: list[tuple[int, int]] = []
    for cp in sorted(cps):
        if out and cp == out[-1][1] + 1:
            out[-1] = (out[-1][0], cp)
        else:
            out.append((cp, cp))
    return out


def css_unicode_range(cps: list[int]) -> str:
    return ",".join(
        f"U+{a:X}" if a == b else f"U+{a:X}-{b:X}" for a, b in to_ranges(cps)
    )


def build_ui() -> list[dict]:
    """把全量 Pan-CJK 字体切成「常用块 + 余量块」，返回分块清单。"""
    src = download(UI_SOURCE, UI_URL)
    font = TTFont(str(src))
    cmap = sorted(font.getBestCmap().keys())
    font.close()
    cmap_set = set(cmap)

    chars = set(ascii_set())
    for key in ("ui", "zh-CN", "zh-TW", "en", "ja"):
        chars |= corpus()[key]
    used = sorted(cp for cp in (ord(c) for c in chars) if cp in cmap_set)
    used_set = set(used)
    rest = [cp for cp in cmap if cp not in used_set]

    groups: list[list[int]] = []
    for i in range(0, len(used), COMMON_CHUNK):
        groups.append(used[i:i + COMMON_CHUNK])
    for i in range(0, len(rest), REST_CHUNK):
        groups.append(rest[i:i + REST_CHUNK])

    # 清理旧分块
    for old in OUT.glob("noto-cjk-c*.woff2"):
        old.unlink()

    manifest = []
    total = 0
    for idx, cps in enumerate(groups):
        name = f"noto-cjk-c{idx:02d}"
        out = OUT / f"{name}.woff2"
        write_subset(src, out, cps)
        size = out.stat().st_size
        total += size
        manifest.append({
            "file": out.name,
            "codepoints": len(cps),
            "unicodeRange": css_unicode_range(cps),
            "bytes": size,
        })
        print(f"  {out.name:26s} {len(cps):5d} 码位  {size / 1024:8.1f} KB")
    print(f"  {'UI 分块合计':26s} {len(cmap)} 码位  {total / 1024 / 1024:8.2f} MB（{len(groups)} 块）")
    return manifest


def write_css(manifest: list[dict]) -> None:
    """生成 fonts.css：UI 分块 @font-face（带 unicode-range）+ 等宽字体。"""
    lines = [
        "/* 构建期生成：scripts/build-fonts.py（勿手改）。",
        " *",
        " * UI：内嵌**全量** Pan-CJK Noto Sans CJK，按 unicode-range 分块：",
        " *   - 任何字符都有字形（不按语料裁剪），生僻字会按需加载对应分块；",
        " *   - 常用字集中在前面的分块，首屏只解码几百 KB，启动不再等 12.9MB。",
        " *   - 单一家族 \"Rut UI\"：中日台字形差异由 locl + html[lang] 选择。",
        " *   - font-display:block：字体就绪前不显示文字，绝不先拿系统字体顶替。",
        " * 等宽：JetBrains Mono / IBM Plex Mono / Source Code Pro（全量，几十 KB）。",
        " */",
    ]
    for entry in manifest:
        lines.append(
            '@font-face{font-family:"Rut UI";'
            f"src:url('../assets/fonts/{entry['file']}') format('woff2');"
            f"font-weight:100 900;font-display:block;"
            f"unicode-range:{entry['unicodeRange']}}}"
        )
    lines += [
        '@font-face{font-family:"Rut Mono JB";src:url(\'../assets/fonts/jetbrains-mono.woff2\') format(\'woff2\');font-weight:100 800;font-display:block}',
        '@font-face{font-family:"Rut Mono SC";src:url(\'../assets/fonts/source-code-pro.woff2\') format(\'woff2\');font-weight:200 900;font-display:block}',
        '@font-face{font-family:"Rut Mono Plex";src:url(\'../assets/fonts/ibm-plex-mono.woff2\') format(\'woff2\');font-weight:400;font-display:block}',
    ]
    CSS.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"  {CSS.name}: {len(manifest)} 个 UI 分块 @font-face")


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    print("UI：Pan-CJK 全量字体分块")
    manifest = build_ui()
    print("等宽：全量转换")
    for name, url in MONO_SOURCES.items():
        src = download(name, url)
        out = OUT / f"{name}.woff2"
        to_woff2(src, out)
        print(f"  {out.name:26s} {out.stat().st_size / 1024:8.1f} KB")
    write_css(manifest)
    CACHE.mkdir(parents=True, exist_ok=True)
    (CACHE / "fonts.manifest.json").write_text(json.dumps({
        "uiSource": UI_SOURCE,
        "commonChunk": COMMON_CHUNK,
        "restChunk": REST_CHUNK,
        "chunks": manifest,
    }, ensure_ascii=False, indent=2), encoding="utf-8")
    print("  manifest: scripts/.fontcache/fonts.manifest.json")


if __name__ == "__main__":
    main()
