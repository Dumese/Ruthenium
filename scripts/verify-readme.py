"""README 结构自检：标题层级、目录锚点、代码围栏、必备章节。

用法: python scripts/verify-readme.py
"""
from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
README = ROOT / "README.md"

# standard-readme 中文约定要求的章节（顺序即期望顺序）
REQUIRED = [
    "背景",
    "安装",
    "用法",
    "相关项目",
    "主要项目负责人",
    "参与贡献方式",
    "贡献人员",
    "开源协议",
]

FENCE = "```"


def main() -> int:
    text = README.read_text(encoding="utf-8")
    heads = re.findall(r"^(#{2,3})\s+(.+)$", text, re.M)
    names = [h[1].replace("（可选）", "").strip() for h in heads]
    anchors = re.findall(r"^\s*-\s+\[([^\]]+)\]\(#([^)]+)\)", text, re.M)

    rc = 0

    # 1) 必备章节齐全且顺序正确
    missing = [s for s in REQUIRED if s not in names]
    if missing:
        print(f"[FAIL] 缺少章节: {missing}")
        rc = 1
    else:
        order = [names.index(s) for s in REQUIRED]
        ok = order == sorted(order)
        print(f"[{'OK' if ok else 'FAIL'}] 章节齐全且顺序正确: {names}")
        rc |= 0 if ok else 1

    # 2) 目录锚点必须指向真实标题（GitHub 规则：小写、空格转连字符）
    gh = {h.lower().replace(" ", "-") for h in names}
    gh |= set(names)
    bad = [(label, target) for label, target in anchors if target not in gh]
    if bad:
        print(f"[FAIL] 目录锚点无效: {bad}")
        rc = 1
    else:
        print(f"[OK] 目录锚点全部有效（{len(anchors)} 项）")

    # 3) 目录项与章节数量匹配（每个必备章节都有入口）
    # 目录里允许写成「相关项目（可选）」，比较前统一去掉括注
    toc_labels = {re.sub(r"（.*?）", "", label).strip() for label, _ in anchors}
    missing_toc = [s for s in REQUIRED if s not in toc_labels]
    if missing_toc:
        print(f"[FAIL] 目录缺项: {missing_toc}")
        rc = 1
    else:
        print("[OK] 目录覆盖全部必备章节")

    # 4) 代码围栏成对
    fences = text.count(FENCE)
    ok = fences % 2 == 0
    print(f"[{'OK' if ok else 'FAIL'}] 代码围栏成对: {fences}")
    rc |= 0 if ok else 1

    # 5) 徽章存在（standard-readme 建议）
    badges = text.count("img.shields.io")
    print(f"[{'OK' if badges else 'WARN'}] 徽章数量: {badges}")

    print(f"字符数: {len(text)}")
    return rc


if __name__ == "__main__":
    sys.exit(main())
