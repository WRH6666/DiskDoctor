"""在 Node 里用最小 DOM 桩真实执行页面 JS —— 抓运行时错误。

为什么需要它：`node --check` 只能验语法。**取不到元素、拼错字段、
在 undefined 上取属性**这类问题只有真的把代码跑起来才会暴露，
而页面里一旦抛错，用户看到的就是"整个列表空白"，且没有任何提示。

这里不依赖 jsdom（那要装包），只实现页面实际用到的那一小部分 DOM。
好处是：用到的 API 一目了然，DOM 桩本身也是一份"这页到底依赖什么"的清单。

用法：
    python scripts/check_survey_runtime.py [html路径]
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

ROOT = _paths.ROOT
DEFAULT_HTML = _paths.survey_html()
NODE = _paths.require_node()

# 最小 DOM 桩。只实现页面用到的成员 —— 缺什么这里会立刻报出来。
DOM_STUB = r"""
// ---------------------------------------------------------------- 元素桩
const calls = { created: [], appended: [], ids: [] };

function makeEl(tag) {
  const el = {
    tagName: (tag || "div").toUpperCase(),
    children: [], _html: "", _text: "",
    style: { setProperty(k, v) { this[k] = v; }, },
    dataset: {},
    classList: {
      _s: new Set(),
      add(...c) { c.forEach(x => this._s.add(x)); },
      remove(...c) { c.forEach(x => this._s.delete(x)); },
      toggle(c, on) { if (on === undefined) { this._s.has(c) ? this._s.delete(c) : this._s.add(c); }
                      else { on ? this._s.add(c) : this._s.delete(c); } },
      contains(c) { return this._s.has(c); },
    },
    appendChild(c) { this.children.push(c); return c; },
    // DocumentFragment 在真实 DOM 里会被"摊平"进父节点。
    // 桩也照做，否则 replaceChildren(frag) 只算 1 个子节点，
    // 行数统计就没有意义了。
    replaceChildren(...c) {
      this.children = [];
      for (const x of c) {
        if (x && x.tagName === "FRAGMENT") this.children.push(...x.children);
        else this.children.push(x);
      }
    },
    querySelector(sel) {
      // 筛选片是 label 包一个 input，页面对它取 querySelector("input")
      if (sel === "input") return makeEl("input");
      return makeEl("div");
    },
    // 关键：页面用 chips.querySelectorAll("input:checked") 读筛选状态。
    // 桩必须如实反映"哪些片被勾上了"，否则筛选集合恒为空、
    // 渲染出 0 行 —— 那是桩的错，不是页面的错。
    querySelectorAll(sel) {
      if (sel === "input:checked") {
        const out = [];
        for (const lbl of this.children) {
          const html = lbl._html || "";
          const m = html.match(/data-k="([^"]+)"/);
          if (m && /\schecked/.test(html)) {
            out.push({ dataset: { k: m[1] }, checked: true, addEventListener() {} });
          }
        }
        return out;
      }
      return [];
    },
    addEventListener() {},
    removeEventListener() {},
    closest() { return null; },
    setAttribute() {},
    getAttribute() { return null; },
    focus() {},
    click() {},
    get innerHTML() { return this._html; },
    set innerHTML(v) { this._html = String(v); },
    get textContent() { return this._text; },
    set textContent(v) { this._text = String(v); },
    get firstChild() { return this.children[0] || null; },
    get parentElement() { return null; },
    getBoundingClientRect() { return { top: 0, left: 0, bottom: 0, right: 0, width: 100, height: 20 }; },
    get previousElementSibling() { return makeEl("div"); },
    hidden: false, disabled: false, checked: false, value: "",
  };
  calls.created.push(el.tagName);
  return el;
}

const registry = new Map();
function reg(id, tag) { const e = makeEl(tag); e.id = id; registry.set(id, e); return e; }

// 与真实 HTML 对齐的元素清单
["meta","cards","comp","notes","chips","cat","list","q","sort","dock-stat",
 "dock-hint","pick-safe","clear","export"].forEach(id => reg(id, "div"));

const document = {
  body: makeEl("body"),
  documentElement: makeEl("html"),
  getElementById(id) {
    calls.ids.push(id);
    if (!registry.has(id)) { throw new Error("getElementById 取不到 #" + id); }
    return registry.get(id);
  },
  createElement: makeEl,
  createDocumentFragment() { return makeEl("fragment"); },
  createRange() { return { selectNodeContents() {}, }; },
  querySelector(sel) {
    if (sel === ".dock") return reg("__dock", "div");
    return makeEl("div");
  },
  querySelectorAll() { return []; },
  addEventListener() {},
  removeEventListener() {},
};
const window = {
  getSelection() { return { removeAllRanges() {}, addRange() {} }; },
  addEventListener() {}, innerWidth: 1440, innerHeight: 900,
  location: { href: "file:///x.html" },
};
const navigator = { clipboard: { writeText() { return Promise.resolve(); } } };
const setTimeout = (fn) => { if (typeof fn === "function") {} return 0; };
const clearTimeout = () => {};
const Image = function () { this.src = ""; };
globalThis.__imgSrcs = [];
Image.prototype = { set src(v) { globalThis.__imgSrcs.push(v); }, get src() { return ""; } };
const Blob = function (p) { this.parts = p; };
globalThis.URL = { createObjectURL() { return "blob:x"; }, revokeObjectURL() {} };
"""

HARNESS = r"""
// ---------------------------------------------------------------- 执行页面 JS
const errors = [];
try {
  __PAGE_JS__
} catch (e) {
  errors.push("加载期抛错: " + (e && e.stack ? e.stack.split("\n").slice(0,3).join(" | ") : e));
}

// 结果输出
const out = {
  errors,
  created_tags: calls.created.length,
  ids_touched: [...new Set(calls.ids)],
  cards_html: registry.get("cards") ? registry.get("cards").children.map(c => c.innerHTML) : [],
  comp_html: registry.get("comp") ? registry.get("comp").innerHTML : "",
  notes_html: registry.get("notes") ? registry.get("notes").children.map(c => c.innerHTML) : [],
  chips_html: registry.get("chips") ? registry.get("chips").children.map(c => c.innerHTML) : [],
    list_html: registry.get("list") ? registry.get("list").innerHTML : "",
    list_children: registry.get("list") ? registry.get("list").children.length : -1,
    list_row_html: registry.get("list")
      ? registry.get("list").children.slice(0, 3).map(c => c._html || "").join("\n---\n")
      : "",
    body_images: globalThis.__imgSrcs,
};
console.log("__RESULT__" + JSON.stringify(out));
"""


def main() -> int:
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_HTML
    if not path.exists():
        print(f"✗ 找不到 {path}")
        return 1
    if not NODE.exists():
        print(f"✗ 找不到 node: {NODE}")
        return 1

    html = path.read_text(encoding="utf-8", errors="replace")
    m = re.search(r"<script>(.*?)</script>", html, re.S)
    if not m:
        print("✗ 找不到 <script>")
        return 1
    page_js = m.group(1)

    script = DOM_STUB + "\n" + HARNESS.replace("__PAGE_JS__", page_js)

    with tempfile.NamedTemporaryFile("w", suffix=".js", delete=False,
                                     encoding="utf-8",
                                     dir=str(_paths.work_dir("runtime"))) as f:
        f.write(script)
        tmp = Path(f.name)

    try:
        r = subprocess.run([str(NODE), str(tmp)], capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=120)
    finally:
        try:
            tmp.unlink()
        except OSError:
            pass

    marker = "__RESULT__"
    if marker not in (r.stdout or ""):
        print("✗ 脚本未能产出结果")
        print("stdout:", (r.stdout or "")[-1500:])
        print("stderr:", (r.stderr or "")[-1500:])
        return 1

    data = json.loads(r.stdout.split(marker, 1)[1].strip())

    problems = 0
    print("── 运行时 ──")
    if data["errors"]:
        print(f"  ✗ 抛错 {len(data['errors'])} 处:")
        for e in data["errors"]:
            print(f"      {e}")
        problems += len(data["errors"])
    else:
        print("  ✓ 全程无异常")
    print(f"  · 创建元素 {data['created_tags']} 个")

    print()
    print("── 顶部汇总 ──")
    print(f"  卡片: {len(data['cards_html'])} 张")
    for h in data["cards_html"]:
        # 抽出关键片段做断言
        has_v = 'class="v"' in h
        has_share = 'class="share"' in h and "<i style=" in h
        label = re.search(r">([^<]+)</div><div class=\"v\"", h)
        size = re.search(r'class="v">([^<]+)<', h)
        print(f"    {'✓' if has_v and has_share else '✗'} "
              f"{(label.group(1) if label else '?')}: "
              f"{(size.group(1) if size else '?')}"
              f"{'' if has_share else '  ← 缺占比条'}")
        if not (has_v and has_share):
            problems += 1

    print()
    print("── 构成条 ──")
    comp = data["comp_html"]
    if comp:
        segs = comp.count("<i ")
        lg = comp.count("<span><span")
        print(f"  ✓ 已渲染：{segs} 段 ｜ 图例 {lg} 项")
        if segs == 0 or lg == 0:
            print("  ✗ 结构不完整")
            problems += 1
        if "NaN" in comp or "undefined" in comp:
            print("  ✗ 含 NaN / undefined")
            problems += 1
    else:
        print("  ⚠ 未渲染（数据不足两档时不画，属正常）")

    print()
    print("── 提示与筛选 ──")
    print(f"  提示 {len(data['notes_html'])} 条 ｜ 筛选片 {len(data['chips_html'])} 个")

    print()
    print("── 列表 ──")
    print(f"  渲染行数: {data['list_children']}")
    if data["list_children"] <= 0:
        print("  ✗ 一行都没渲染出来")
        problems += 1
    else:
        print("  ✓ 有内容")
    if "NaN" in data["list_html"]:
        print("  ✗ 列表里有 NaN")
        problems += 1
    if "undefined" in data["list_html"]:
        print("  ✗ 列表里有 undefined")
        problems += 1

    # 抽样检查行结构：路径可点、体积在、勾选框状态合理
    row = data.get("list_row_html", "")
    if row:
        checks = [
            ("路径可点击（.pth 带 data-path）",
             'class="pth"' in row and "data-path=" in row),
            ("体积块存在", '<div class="size">' in row),
            ("未选中的行没有 picked 类", "row picked" not in row.split("\n")[0]),
        ]
        print()
        print("  行结构抽样:")
        for name, ok in checks:
            print(f"    {'✓' if ok else '✗'} {name}")
            if not ok:
                problems += 1

    print()
    print("=" * 60)
    print("✓ 运行时全部通过" if problems == 0 else f"✗ 发现 {problems} 处问题")
    print("=" * 60)
    return 0 if problems == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
