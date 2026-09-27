"""检查生成的界面里的 CSS 与结构一致性。

为什么需要它：CSS 出错**几乎从不报错**，只是悄悄不生效。
最典型的一类是"引用了未定义的自定义属性" —— 有 fallback 值时看起来没问题，
没有 fallback 时整条声明被丢弃，而页面上只是"某处颜色不对"，排查起来很费眼。

本脚本检查：
  1. 所有 `var(--x)` 引用的变量都有定义（无 fallback 的必须定义）
  2. 花括号配平
  3. JS 里出现的 `class="..."` / `className = "..."` 都能在 CSS 里找到规则
  4. JS 里 `getElementById("x")` 的每个 id 都在 HTML 里存在

用法：
    python scripts/check_survey_css.py [html路径]
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

DEFAULT_HTML = _paths.survey_html()


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_HTML
    if not path.exists():
        print(f"✗ 找不到 {path}")
        return 1
    html = path.read_text(encoding="utf-8", errors="replace")

    m = re.search(r"<style>(.*?)</style>", html, re.S)
    if not m:
        print("✗ 找不到 <style> 块")
        return 1
    css = m.group(1)

    js_m = re.search(r"<script>(.*?)</script>", html, re.S)
    js = js_m.group(1) if js_m else ""

    problems = 0

    # ---------------------------------------------------------- 1. 变量
    defined = set(re.findall(r"(--[\w-]+)\s*:", css))
    # JS 也会在运行时注入变量（例如按数据给每张卡设一个语义色）
    defined |= set(re.findall(r'setProperty\(\s*"(--[\w-]+)"', js))
    used: list[tuple[str, bool]] = []
    for mm in re.finditer(r"var\(\s*(--[\w-]+)\s*(,)?", css):
        used.append((mm.group(1), bool(mm.group(2))))

    missing_nodefault = sorted({n for n, has_fb in used if n not in defined and not has_fb})
    missing_withdefault = sorted({n for n, has_fb in used if n not in defined and has_fb})

    print("── CSS 自定义属性 ──")
    print(f"  已定义: {len(defined)} 个")
    if missing_nodefault:
        print(f"  ✗ 引用了但未定义、且没有 fallback（该声明会被丢弃）:")
        for n in missing_nodefault:
            print(f"      {n}")
        problems += len(missing_nodefault)
    else:
        print("  ✓ 没有'引用未定义且无 fallback'的变量")

    if missing_withdefault:
        print(f"  ⚠ 引用了未定义但有 fallback（能跑，但说明变量清单不一致）:")
        for n in missing_withdefault:
            print(f"      {n}")
        problems += len(missing_withdefault)

    unused = sorted(defined - {n for n, _ in used})
    if unused:
        print(f"  · 已定义但未被引用: {', '.join(unused)}")

    # ---------------------------------------------------------- 2. 花括号
    print()
    print("── CSS 结构 ──")
    depth = 0
    for ch in css:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
    if depth != 0:
        print(f"  ✗ 花括号不配平，差 {depth}")
        problems += 1
    else:
        print("  ✓ 花括号配平")

    # ---------------------------------------------------------- 3. 类名
    css_classes = set(re.findall(r"\.([A-Za-z][\w-]*)", css))
    # JS 里构造的 class 字符串（class="..." 与 className = "..."）
    js_classes: set[str] = set()
    for mm in re.finditer(r'class(?:Name)?\s*=\s*"([^"]*)"', js):
        # 模板里可能含 ${...}，先去掉
        raw = re.sub(r"\$\{[^}]*\}", " ", mm.group(1))
        js_classes.update(t for t in raw.split() if t)
    # classList.add / toggle 里的
    for mm in re.finditer(r'classList\.(?:add|toggle|remove)\(\s*"([^"]+)"', js):
        js_classes.update(mm.group(1).split())

    orphan = sorted(
        c for c in js_classes
        # 排除浏览器内置（没有样式也正常）
        if c not in css_classes and c not in {"on", "open", "show", "picked", "diff", "aggr", "locked"}
    )
    print()
    print("── 类名对应 ──")
    print(f"  CSS 定义: {len(css_classes)} 个 ｜ JS 使用: {len(js_classes)} 个")
    if orphan:
        print(f"  ⚠ JS 用到但 CSS 里没有规则（可能是拼错，也可能是有意无样式）:")
        for c in orphan:
            print(f"      .{c}")
    else:
        print("  ✓ JS 用到的类名都有对应规则")

    # ---------------------------------------------------------- 4. id
    ids_in_html = set(re.findall(r'id="([\w-]+)"', html))
    # JS 也可能在运行时创建元素并赋 id（例如浮层、提示条）——
    # 这类不算"引用了不存在的 id"，否则会误报
    ids_made_in_js = set(re.findall(r'\.id\s*=\s*"([\w-]+)"', js))
    ids_in_js = set(re.findall(r'getElementById\("([\w-]+)"\)', js))
    missing_ids = sorted(ids_in_js - ids_in_html - ids_made_in_js)
    print()
    print("── 元素 id ──")
    print(f"  HTML 里: {len(ids_in_html)} 个 ｜ JS 运行时创建: {len(ids_made_in_js)} 个"
          f" ｜ JS 引用: {len(ids_in_js)} 个")
    if missing_ids:
        print(f"  ✗ JS 引用了不存在的 id（会抛 null 错误）:")
        for i in missing_ids:
            print(f"      #{i}")
        problems += len(missing_ids)
    else:
        print("  ✓ JS 引用的 id 都存在")

    print()
    print("=" * 60)
    if problems == 0:
        print("✓ 全部通过")
    else:
        print(f"✗ 发现 {problems} 处问题")
    print("=" * 60)
    return 0 if problems == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
