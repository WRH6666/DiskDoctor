#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
DiskDoctor 参考实现（Reference Implementation）。

这不是最终产品 —— 最终产品是 diskdoctor/ 下的 Rust 工程。
这里存在的意义是：它**完全复刻 Rust 端的设计**（逐层并行 BFS、
倒序 rollup、单趟归因+继承、区域根归并、分档聚合），因此可以在
Rust 工具链还没就绪时，端到端验证这套设计在真实数据上是否成立。

它和 Rust 端共用同一份规则库 crates/dd-rules/rules/rules.yaml。

用法:
    python scripts/dd_proto.py C:\\ --out report.md
    python scripts/dd_proto.py "C:\\Users" --top 15
"""

import argparse
import ctypes
import os
import sys
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from validate_rules import load_rules, rel_lower  # noqa: E402

ROOT_PARENT = -1
UNATTRIBUTED = None
FILE_ATTRIBUTE_REPARSE_POINT = 0x400
FILE_ATTRIBUTE_SPARSE_FILE = 0x200
FILE_ATTRIBUTE_COMPRESSED = 0x800

SAFETY_ORDER = ["safe", "caution", "keep", "system-managed", "protected"]
SAFETY_LABEL = {
    "safe": "可安全回收",
    "caution": "需确认",
    "keep": "用户数据",
    "system-managed": "系统托管",
    "protected": "禁止触碰",
}
ACTION_LABEL = {
    "delete": "清理（移入暂存区，可撤销）",
    "command": "执行官方命令",
    "compact": "压缩虚拟磁盘（不丢数据）",
    "review": "需要人工确认",
    "none": "不做处理",
}


# ------------------------------------------------------------------ 物理占用

GetCompressedFileSizeW = ctypes.windll.kernel32.GetCompressedFileSizeW
GetCompressedFileSizeW.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_uint32)]
GetCompressedFileSizeW.restype = ctypes.c_uint32


def physical_size(path, logical, attrs):
    """
    只为稀疏文件和压缩文件求真实物理占用。

    每个文件都调一次会让扫描明显变慢，而只有稀疏/压缩文件才会显著偏离
    逻辑大小 —— 虚拟磁盘镜像（WSL 的 ext4.vhdx、Docker 的 vhdx）正好属于
    这类，而它们恰恰是用户最想看清的"幽灵占用"。
    """
    if not (attrs & FILE_ATTRIBUTE_SPARSE_FILE or attrs & FILE_ATTRIBUTE_COMPRESSED):
        return logical
    high = ctypes.c_uint32(0)
    low = GetCompressedFileSizeW(path, ctypes.byref(high))
    if low == 0xFFFFFFFF:
        return logical
    return (high.value << 32) | low


# ------------------------------------------------------------------ 扫描

class Entry:
    __slots__ = ("name", "parent", "size", "alloc", "mtime", "is_dir")

    def __init__(self, name, parent, size, alloc, mtime, is_dir):
        self.name = name
        self.parent = parent
        self.size = size
        self.alloc = alloc
        self.mtime = mtime
        self.is_dir = is_dir


def read_children(path):
    """返回 (子项列表, 跳过数)。"""
    out = []
    skipped = 0
    try:
        with os.scandir(path) as it:
            for de in it:
                try:
                    st = de.stat(follow_symlinks=False)
                except OSError:
                    skipped += 1
                    continue
                attrs = getattr(st, "st_file_attributes", 0)
                is_dir = de.is_dir(follow_symlinks=False)
                is_reparse = bool(attrs & FILE_ATTRIBUTE_REPARSE_POINT)
                if is_dir:
                    size = alloc = 0
                else:
                    size = st.st_size
                    alloc = physical_size(de.path, size, attrs)
                out.append((
                    de.name,
                    size,
                    alloc,
                    int(st.st_mtime),
                    is_dir,
                    is_dir and not is_reparse,
                ))
    except OSError:
        return out, 1
    return out, skipped


def scan(root, workers=16):
    t0 = time.time()
    root = os.path.abspath(root)
    entries = [Entry(os.path.basename(root) or root, ROOT_PARENT, 0, 0, 0, True)]
    skipped = 0

    frontier = [(0, root)]
    depth = 0
    while frontier and depth < 512:
        depth += 1
        nxt = []
        with ThreadPoolExecutor(max_workers=workers) as ex:
            results = list(ex.map(lambda t: read_children(t[1]), frontier))
        for (parent_idx, parent_path), (children, sk) in zip(frontier, results):
            skipped += sk
            for name, size, alloc, mtime, is_dir, descend in children:
                idx = len(entries)
                entries.append(Entry(name, parent_idx, size, alloc, mtime, is_dir))
                if descend:
                    nxt.append((idx, os.path.join(parent_path, name)))
        frontier = nxt

    # 倒序 rollup：父节点下标恒小于子节点下标，所以倒着扫一遍即可
    for i in range(len(entries) - 1, 0, -1):
        e = entries[i]
        p = entries[e.parent]
        p.size += e.size
        p.alloc += e.alloc

    return entries, time.time() - t0, skipped


def normalize_root_prefix(p):
    """
    扫描根的"去盘符绝对路径"，小写。

    `C:\\`            -> ``
    `C:\\Users\\me`    -> `users\\me`
    `C:\\Windows`      -> `windows`

    路径规则对着 `root_prefix + 相对路径` 求值，好处是三种扫描方式
    都能命中同一条规则：
        扫 C:\\                -> users\\me\\appdata\\local\\temp
        扫 C:\\Users\\me       -> users\\me\\appdata\\local\\temp
        扫 C:\\Windows         -> windows\\temp
    如果只用"相对扫描根的路径"，扫描 C:\\Windows 时 `windows-temp`
    这类规则就永远匹配不上了。
    """
    s = os.path.abspath(p).replace("/", "\\")
    if len(s) >= 2 and s[1] == ":":
        s = s[2:]
    return s.strip("\\").lower()


def dir_path_lower(entries, idx, root_prefix):
    parts = []
    while idx != ROOT_PARENT and idx != 0:
        parts.append(entries[idx].name.lower())
        idx = entries[idx].parent
    parts.reverse()
    rel = "\\".join(parts)
    if not rel:
        return root_prefix
    return f"{root_prefix}\\{rel}" if root_prefix else rel


def abs_path(entries, root, idx):
    """绝对路径。注意跳过下标 0（扫描根自身），否则会拼出
    `C:\\Users\\me\\me\\AppData\\...` 这种重复用户名的路径。"""
    parts = []
    while idx != ROOT_PARENT and idx != 0:
        parts.append(entries[idx].name)
        idx = entries[idx].parent
    parts.reverse()
    return os.path.join(root, *parts) if parts else root


# ------------------------------------------------------------------ 归因

def attribute_all(entries, rules, root_prefix):
    """
    单趟归因，与 Rust 端一致：

    - 每条路径取「自己的规则」与「从父目录继承」中 priority 更高的那个
    - 自己没命中就继承

    目录也必须继承 —— 否则 `path-suffix` 只能命中恰好等于该路径的那个目录，
    `C:\\Windows\\WinSxS\\Manifests` 这类子目录会全部掉进"未识别"。
    """
    n = len(entries)
    attrib = [UNATTRIBUTED] * n
    needs_path = any(r.is_path_rule for r in rules)

    for i in range(n):
        e = entries[i]
        parent_attr = attrib[e.parent] if e.parent != ROOT_PARENT else UNATTRIBUTED

        own = None
        if e.is_dir:
            lower = dir_path_lower(entries, i, root_prefix) if needs_path else ""
            name = e.name.lower()
            for r in rules:
                if r.match_dir_at(name, lower):
                    own = r
                    break
        else:
            name = e.name.lower()
            for r in rules:
                if r.match_file(name):
                    own = r
                    break

        if own is None:
            attrib[i] = parent_attr
        elif parent_attr is None:
            attrib[i] = own
        else:
            attrib[i] = own if own.priority >= parent_attr.priority else parent_attr

    return attrib


# ------------------------------------------------------------------ 聚合

class Finding:
    pass


def analyze(entries, root, attrib, rules):
    """
    归并成互不重叠的"区域"，并算出各自的体积。

    # 为什么不能直接累加区域根的体积

    区域根是**嵌套**的：`AppData\\Local` 是一个区域根，`AppData\\Local\\Temp`
    是它内部另一个区域根。而目录的 size 是整棵子树的量，直接相加就把
    Temp 算了两遍 —— 表现为"归因覆盖率 151%"和负数的"未识别"。

    正确做法是拆成两件事：
      1. `ex`（独占大小）：目录的 size 减去所有直接子项，剩下的才是
         "只属于它自己"的部分。所有 ex 相加恰好等于总量。
      2. `owner`：覆盖每个条目的那个区域根。
    然后每个区域根只累加 `owner` 指向它的那些条目的 ex，区域之间就
    天然不重叠，全部区域根的体积之和 = 已归因总量。
    """
    n = len(entries)

    ex = [e.size for e in entries]
    for i in range(1, n):
        ex[entries[i].parent] -= entries[i].size

    owner = [-1] * n
    for i in range(n):
        if attrib[i] is None:
            owner[i] = -1
            continue
        p = entries[i].parent
        if p != ROOT_PARENT and attrib[p] is attrib[i]:
            owner[i] = owner[p]
        else:
            owner[i] = i

    sizes = defaultdict(int)
    files_count = defaultdict(int)
    for i in range(n):
        o = owner[i]
        if o == -1:
            continue
        sizes[o] += ex[i]
        if not entries[i].is_dir:
            files_count[o] += 1

    findings = []
    for o, sz in sizes.items():
        if sz <= 0:
            continue
        f = Finding()
        f.rule = attrib[o]
        f.path = abs_path(entries, root, o)
        f.size = sz
        f.alloc = entries[o].alloc
        f.is_dir = entries[o].is_dir
        f.files = files_count.get(o, 0)
        findings.append(f)

    attributed = sum(sizes.values())
    total = entries[0].size if entries else 0

    # 未识别区域的根，用来告诉用户"规则库漏了什么"。
    # 这里刻意不把扫描根的直接子项并成一大坨 —— 那样只会得到
    # 「未识别 19.6GB」一句话，对补规则毫无帮助。根的直接子项各自成区。
    unk_owner = [-1] * n
    for i in range(n):
        if attrib[i] is not None:
            continue
        p = entries[i].parent
        if p > 0 and attrib[p] is None:
            unk_owner[i] = unk_owner[p]
        else:
            unk_owner[i] = i

    unk_sizes = defaultdict(int)
    unk_files = defaultdict(int)
    for i in range(n):
        o = unk_owner[i]
        if o == -1:
            continue
        unk_sizes[o] += ex[i]
        if not entries[i].is_dir:
            unk_files[o] += 1

    unknowns = [
        (abs_path(entries, root, o), sz, unk_files.get(o, 0))
        for o, sz in unk_sizes.items()
        if sz > 0
    ]
    unknowns.sort(key=lambda x: -x[1])

    return findings, attributed, total - attributed, unknowns[:15]


# ------------------------------------------------------------------ 报告

def human(n):
    if n < 1024:
        return f"{n} B"
    for unit, div in (("KB", 1024), ("MB", 1024 ** 2), ("GB", 1024 ** 3), ("TB", 1024 ** 4)):
        if n < div * 1024 or unit == "TB":
            return f"{n / div:.2f} {unit}"
    return f"{n} B"


def fmt_num(n):
    return f"{n:,}"


def render(entries, root, findings, unattributed, rules, elapsed, skipped, top, unknowns):
    total = entries[0].size if entries else 0
    files = sum(1 for e in entries if not e.is_dir)
    dirs = sum(1 for e in entries if e.is_dir)

    by_safety = defaultdict(lambda: [0, 0, 0])  # size, files, items
    by_cat = defaultdict(lambda: [0, 0, 0, None])
    for f in findings:
        s = by_safety[f.rule.safety]
        s[0] += f.size
        s[1] += f.files
        s[2] += 1
        c = by_cat[f.rule.category]
        c[0] += f.size
        c[1] += f.files
        c[2] += 1
        if c[3] is None or SAFETY_ORDER.index(f.rule.safety) > SAFETY_ORDER.index(c[3]):
            c[3] = f.rule.safety

    L = []
    A = L.append
    A("# 磁盘体检报告")
    A("")
    A(f"> 扫描路径 `{root}` ｜ 参考实现 ｜ 耗时 {elapsed:.1f}s ｜ 规则 {len(rules)} 条")
    A("")

    A("## 结论")
    A("")
    A(f"- 本次扫到 **{fmt_num(files)}** 个文件、**{fmt_num(dirs)}** 个目录，合计 **{human(total)}**")
    safe = by_safety.get("safe", [0, 0, 0])
    A(f"- **可安全回收：{human(safe[0])}** —— 删了能自动重建或重新下载，共 {safe[2]} 处")
    for k in ("system-managed", "caution", "keep"):
        v = by_safety.get(k)
        if v and v[0] > 0:
            A(f"- {SAFETY_LABEL[k]}：**{human(v[0])}**，共 {v[2]} 处")
    if unattributed > 0:
        A(f"- 规则库未覆盖：**{human(unattributed)}** —— 需要人工判断")
    A("")

    for k in ("safe", "caution"):
        items = sorted([f for f in findings if f.rule.safety == k],
                       key=lambda f: -f.size)[:top]
        title = "可以放心清理的" if k == "safe" else "需要你确认的"
        A(f"## {title}（按体积降序，前 {len(items)} 项）")
        A("")
        if not items:
            A("无。")
            A("")
            continue
        A("| 体积 | 项目 | 位置 | 文件数 |")
        A("|---|---|---|---|")
        for f in items:
            loc = f.path if len(f.path) <= 70 else f.path[:34] + "…" + f.path[-34:]
            A(f"| **{human(f.size)}** | {f.rule.name} | `{loc}` | {fmt_num(f.files)} |")
        A("")
        for f in items[: min(8, len(items))]:
            A(f"### {human(f.size)} — {f.rule.name}")
            A("")
            A(f"- 路径：`{f.path}`")
            A(f"- 内容：{fmt_num(f.files)} 个文件，{f.rule.category}")
            A(f"- 为什么能删：{f.rule.raw.get('why', '')}")
            hint = ACTION_LABEL.get(f.rule.action, f.rule.action)
            if f.rule.action == "command" and f.rule.command:
                hint += f"：`{f.rule.command}`"
            A(f"- 怎么删：{hint}")
            A(f"- 删了怎么恢复：{f.rule.raw.get('recovery', '')}")
            A("")

    A("## 按类别汇总")
    A("")
    A("| 类别 | 体积 | 文件数 | 处数 | 最保守口径 |")
    A("|---|---|---|---|---|")
    for cat, v in sorted(by_cat.items(), key=lambda x: -x[1][0]):
        A(f"| {cat} | {human(v[0])} | {fmt_num(v[1])} | {fmt_num(v[2])} | {SAFETY_LABEL[v[3]]} |")
    A("")

    if unknowns:
        A(f"## 未识别 TOP {len(unknowns)}（规则库待补充）")
        A("")
        A("这些路径规则库没有覆盖，工具无法判断能不能删。")
        A("但它们不是不重要 —— 恰恰相反，这是最需要你去确认的部分。")
        A("")
        A("| 体积 | 位置 | 文件数 |")
        A("|---|---|---|")
        for p, sz, fc in unknowns:
            loc = p if len(p) <= 80 else p[:38] + "…" + p[-38:]
            A(f"| **{human(sz)}** | `{loc}` | {fmt_num(fc)} |")
        A("")

    if skipped:
        A(f"> 提示：有 {fmt_num(skipped)} 处路径读取失败（多为权限不足）。"
          "以管理员身份运行可获得完整数据。")
        A("")
    A("---")
    A("")
    A("本报告为**只读分析**，没有删除或修改任何文件。")
    return "\n".join(L)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--out")
    ap.add_argument("--top", type=int, default=20)
    ap.add_argument("--workers", type=int, default=16)
    args = ap.parse_args()

    rules = load_rules(Path(__file__).resolve().parent.parent
                       / "diskdoctor" / "crates" / "dd-rules" / "rules" / "rules.yaml")

    root = os.path.abspath(args.path)
    print(f"DiskDoctor 参考实现 · 规则 {len(rules)} 条")
    print(f"扫描 {root} …")
    entries, elapsed, skipped = scan(root, args.workers)
    print(f"完成：{len(entries)} 个条目，{elapsed:.1f}s，跳过 {skipped} 处")

    root_prefix = normalize_root_prefix(root)
    attrib = attribute_all(entries, rules, root_prefix)
    findings, attributed, unattributed, unknowns = analyze(entries, root, attrib, rules)

    report = render(entries, root, findings, unattributed, rules, elapsed, skipped, args.top, unknowns)

    if args.out:
        Path(args.out).write_text(report, encoding="utf-8")
        print(f"报告已写入 {args.out}")

    # 控制台速览
    by_safety = defaultdict(int)
    for f in findings:
        by_safety[f.rule.safety] += f.size
    print()
    for k in SAFETY_ORDER:
        if by_safety.get(k):
            print(f"  {SAFETY_LABEL[k]:<8} {human(by_safety[k]):>12}")
    if unattributed:
        print(f"  {'未识别':<8} {human(unattributed):>12}")
    print()
    print(f"  归因覆盖率 {100.0 * attributed / entries[0].size:.1f}%"
          if entries and entries[0].size else "")
    return 0


if __name__ == "__main__":
    sys.exit(main())
