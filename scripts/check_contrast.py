"""配色对比度检查 —— 深色主题下最容易犯的错是把字调得太暗。

为什么值得单独一个脚本：改配色时"看起来还行"和"实际能不能读"是两件事，
而深色底上的低对比度在亮屏上看尤其吃力。这类问题**不会报错**，
只是有人看着累、或者看不清次要信息。

阈值（WCAG 2.1）：
  · 正文 / 小字      ≥ 4.5:1
  · 大字 / 图形 / 徽章 ≥ 3.0:1

用法：
    python scripts/check_contrast.py
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

SURVEY = _paths.SURVEY_RS

AA_NORMAL = 4.5
AA_LARGE = 3.0


def luminance(hex_color: str) -> float:
    r, g, b = (int(hex_color[i:i + 2], 16) / 255 for i in (1, 3, 5))

    def lin(c: float) -> float:
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)


def ratio(fg: str, bg: str) -> float:
    a, b = luminance(fg), luminance(bg)
    hi, lo = max(a, b), min(a, b)
    return (hi + 0.05) / (lo + 0.05)


def grade(r: float) -> str:
    if r >= 7:
        return "AAA"
    if r >= AA_NORMAL:
        return "AA"
    if r >= AA_LARGE:
        return "AA(大字)"
    return "不足"


def main() -> int:
    if not SURVEY.exists():
        print(f"✗ 找不到 {SURVEY}")
        return 1

    src = SURVEY.read_text(encoding="utf-8")
    # 必须先剥掉注释：注释里常举例子（"白字在 #4a90f7 上只有 3.17:1"），
    # 不剥的话会把这些"举例用的色值"当成真的配色。
    nc = re.sub(r"/\*.*?\*/", "", src, flags=re.S)

    root = re.search(r":root \{(.*?)\n  \}", nc, re.S)
    if not root:
        print("✗ 找不到 :root 令牌块")
        return 1
    tok = {
        m.group(1): m.group(2)
        for m in re.finditer(r"(--[\w-]+):\s*(#[0-9a-fA-F]{6})", root.group(1))
    }

    problems = 0

    print("=== 设计令牌 ===")
    for k, v in sorted(tok.items()):
        print(f"  {k:<14} {v}")

    print()
    print(f"=== 正文（阈值 {AA_NORMAL}）===")
    text_cases = [
        ("--fg 在 --bg", tok.get("--fg"), tok.get("--bg")),
        ("--fg 在 --panel", tok.get("--fg"), tok.get("--panel")),
        ("--fg-dim 在 --panel", tok.get("--fg-dim"), tok.get("--panel")),
        ("--fg-dim 在 --bg", tok.get("--fg-dim"), tok.get("--bg")),
        ("--fg-faint 在 --panel", tok.get("--fg-faint"), tok.get("--panel")),
        ("--fg-faint 在 --panel-3", tok.get("--fg-faint"), tok.get("--panel-3")),
        ("--fg-faint 在 --bg", tok.get("--fg-faint"), tok.get("--bg")),
    ]
    for name, fg, bg in text_cases:
        if not fg or not bg:
            continue
        r = ratio(fg, bg)
        ok = r >= AA_NORMAL
        if not ok:
            problems += 1
        print(f"  {'✓' if ok else '✗'} {name:<26} {r:>6.2f}:1  {grade(r)}")

    print()
    print("=== 底栏主按钮（阈值 4.5）===")
    m = re.search(r"\n  button\.primary \{", nc)
    if m:
        blk = nc[m.start(): nc.index("}", m.start())]
        inks = re.findall(r"#[0-9a-fA-F]{6}", blk)
        nav = tok.get("--accent", "#000000")
        if inks:
            r = ratio(inks[0], nav)
            ok = r >= AA_NORMAL
            if not ok:
                problems += 1
            print(f"  {'✓' if ok else '✗'} 按钮字 {inks[0]} 在 accent 上   {r:>6.2f}:1  {grade(r)}")
        h = re.search(r"\n  button\.primary:hover[^{]*\{([^}]*)\}", nc)
        if h and inks:
            hx = re.findall(r"#[0-9a-fA-F]{6}", h.group(1))
            if hx:
                r = ratio(inks[0], hx[0])
                ok = r >= AA_NORMAL
                if not ok:
                    problems += 1
                print(f"  {'✓' if ok else '✗'} 悬停态                  {r:>6.2f}:1  {grade(r)}")

    print()
    print(f"=== 语义色在面板上（阈值 {AA_LARGE}）===")
    for k in ("--safe", "--caution", "--keep", "--system", "--protected", "--accent"):
        if k not in tok:
            continue
        r = ratio(tok[k], tok["--panel"])
        ok = r >= AA_LARGE
        if not ok:
            problems += 1
        print(f"  {'✓' if ok else '✗'} {k:<14} {r:>6.2f}:1")

    print()
    print("=" * 58)
    if problems == 0:
        print("✓ 对比度全部达标")
    else:
        print(f"✗ {problems} 项不达标 —— 深色底上这会直接影响可读性")
    print("=" * 58)
    return 0 if problems == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
