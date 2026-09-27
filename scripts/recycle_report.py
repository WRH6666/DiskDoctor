"""盘点回收站里都是些什么文件。

## 数据来源

Windows 回收站每个条目由一对文件组成（在自己账户的 SID 子目录下）：

  $I<ID>  元数据：原路径、文件大小、删除时间（本脚本读的就是它）
  $R<ID>  实际数据内容

解析 $I 即可在不触碰数据本体的前提下，统计出「删了什么、从哪删的、多大」。

## 性能考虑

条目数以十万计，逐文件打开会成为瓶颈。两点优化：

  1. 用线程池并行（IO 密集，GIL 在阻塞式 read 时会释放）
  2. 一次性 read() 整个 $I 文件（它很小，几百字节），
     避免 read(28) + read(plen) 两次系统调用

另外 `os.scandir` 在 Windows 上经由 FindFirstFile/FindNextFile 返回的
stat 信息是**免费**的，所以可以先用它做粗筛而不额外付系统调用代价。
"""

import os
import struct
import sys
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor

# 让脚本在非中文 Windows 上也能跑：英文环境的控制台代码页是 cp1252，
# 直接 print 中文会抛 UnicodeEncodeError。
#
# 这里不复用 scripts/_paths.py 里的同名函数，是为了让本脚本保持**自包含** ——
# 它可以被单独拷到任何地方运行，不依赖仓库里的其它文件。
for _s in (sys.stdout, sys.stderr):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError, OSError):
        pass

VOL = "C:\\"
MB = 1024 * 1024
GB = 1024 ** 3

# 回收站目录不写死 —— 用 find_recycle_dirs() 扫 $Recycle.Bin 下的 SID 子目录。
# （原本这里硬编码了一个 SID，等于把自己的机器标识写进仓库，而且那段代码
#   实际从未被调用。）


def find_recycle_dirs():
    """列出所有账户的回收站目录（可能不止一个 SID）。"""
    base = r"C:\$Recycle.Bin"
    out = []
    try:
        for e in os.scandir(base):
            if e.is_dir():
                out.append(e.path)
    except OSError as ex:
        print(f"无法列出 {base}: {ex}")
    return out


# ---------------------------------------------------------------- $I 解析

def parse_i(path):
    """返回 (大小, 删除时间戳, 原始路径)，失败返回 None。"""
    try:
        with open(path, "rb") as f:
            d = f.read()
    except OSError:
        return None
    if len(d) < 28:
        return None
    try:
        _ver = struct.unpack_from("<Q", d, 0)[0]
        size = struct.unpack_from("<Q", d, 8)[0]
        ft = struct.unpack_from("<Q", d, 16)[0]
        plen = struct.unpack_from("<I", d, 24)[0]
        if plen <= 0 or plen > 4096:
            return None
        raw = d[28:28 + plen * 2]
        name = raw.decode("utf-16-le", errors="replace").rstrip("\x00")
        when = (ft - 116444736000000000) / 10_000_000 if ft else 0
        return size, when, name
    except (struct.error, ValueError):
        return None


# ---------------------------------------------------------------- 分类

CATEGORIES = [
    # (类别名, 判定函数)  —— 顺序即优先级，越靠前越先匹配
    ("我的测试/构建残留", lambda p: (
        "workbuddy" in p or "diskdoctor" in p or "_diag" in p
        or "recycle-test" in p or "space-test" in p or "recycle-detect" in p
    )),
    ("用户文档", lambda p: "\\documents\\" in p),
    ("桌面", lambda p: "\\desktop\\" in p),
    ("下载", lambda p: "\\downloads\\" in p),
    ("图片 / 视频", lambda p: "\\pictures\\" in p or "\\videos\\" in p),
    ("音乐", lambda p: "\\music\\" in p),
    ("代码与依赖", lambda p: (
        "node_modules" in p or "\\target\\" in p or ".cargo" in p
        or "__pycache__" in p or ".venv" in p or "site-packages" in p
        or "\\.git\\" in p or "\\dist\\" in p or "\\build\\" in p
    )),
    ("缓存 / 临时", lambda p: (
        "\\cache" in p or "\\temp" in p or "\\tmp" in p
        or "\\logs\\" in p or ".log" in p
    )),
    ("程序与安装包", lambda p: (
        "\\program files" in p or "\\programs\\" in p
        or p.endswith(".exe") or p.endswith(".msi") or p.endswith(".zip")
        or p.endswith(".7z") or p.endswith(".rar")
    )),
    ("AppData 其他", lambda p: "\\appdata\\" in p),
]


def classify(path_lower):
    for name, test in CATEGORIES:
        try:
            if test(path_lower):
                return name
        except Exception:
            pass
    return "其他"


def hsize(n):
    if n >= GB:
        return f"{n / GB:.2f} GB"
    if n >= MB:
        return f"{n / MB:.1f} MB"
    if n >= 1024:
        return f"{n / 1024:.1f} KB"
    return f"{n} B"


# ---------------------------------------------------------------- 主流程

def main():
    root = None
    for d in find_recycle_dirs():
        if d.endswith("-1001"):
            root = d
            break
    if root is None:
        dirs = find_recycle_dirs()
        if not dirs:
            print("没找到回收站目录")
            return
        root = dirs[0]

    print(f"回收站目录: {root}")
    sys.stdout.flush()

    # 收集 $I 文件（只 stat，不读内容）
    t0 = time.time()
    entries = []
    scan_total = 0
    for e in os.scandir(root):
        scan_total += 1
        if e.name.startswith("$I"):
            entries.append(e.path)
    print(f"目录共 {scan_total:,} 项，其中 $I 元数据 {len(entries):,} 个"
          f"（枚举耗时 {time.time() - t0:.1f}s）")
    sys.stdout.flush()

    if not entries:
        print("没有可解析的条目")
        return

    # 并行解析
    t0 = time.time()
    rows = []
    done = 0
    step = max(1, len(entries) // 10)
    with ThreadPoolExecutor(max_workers=64) as pool:
        for r in pool.map(parse_i, entries, chunksize=256):
            done += 1
            if r is not None:
                rows.append(r)
            if done % step == 0:
                pct = done * 100 // len(entries)
                print(f"  解析进度 {pct}%  ({done:,}/{len(entries):,})"
                      f"  已用时 {time.time() - t0:.0f}s")
                sys.stdout.flush()

    print(f"成功解析 {len(rows):,} / {len(entries):,} 条"
          f"（耗时 {time.time() - t0:.1f}s）")
    if not rows:
        return

    total_size = sum(r[0] for r in rows)

    # 按类别汇总
    agg = defaultdict(lambda: {"n": 0, "bytes": 0})
    for size, _when, name in rows:
        c = classify(name.lower())
        agg[c]["n"] += 1
        agg[c]["bytes"] += size

    print()
    print("=" * 78)
    print("按类别汇总（体积降序）")
    print("=" * 78)
    print(f"{'类别':<20}{'条目数':>10}{'合计体积':>14}{'占比':>9}")
    print("-" * 78)
    for c, v in sorted(agg.items(), key=lambda kv: -kv[1]["bytes"]):
        pct = v["bytes"] / total_size * 100 if total_size else 0
        print(f"{c:<20}{v['n']:>10,}{hsize(v['bytes']):>14}{pct:>8.1f}%")
    print("-" * 78)
    print(f"{'合计':<20}{len(rows):>10,}{hsize(total_size):>14}{100:>8.1f}%")

    # 最大的一批
    print()
    print("=" * 78)
    print("体积最大的 25 项")
    print("=" * 78)
    print(f"{'删除时间':<12}{'体积':>11}  原始路径")
    print("-" * 78)
    for size, when, name in sorted(rows, key=lambda r: -r[0])[:25]:
        ts = time.strftime("%m-%d %H:%M", time.localtime(when)) if when else "?"
        p = name if len(name) <= 52 else f"...{name[-49:]}"
        print(f"{ts:<12}{hsize(size):>11}  {p}")

    # 我造成的残留
    print()
    print("=" * 78)
    print("其中：本次工作造成的残留")
    print("=" * 78)
    mine = [r for r in rows if classify(r[2].lower()) == "我的测试/构建残留"]
    if mine:
        mt = sum(r[0] for r in mine)
        print(f"共 {len(mine):,} 条，合计 {hsize(mt)}")
        print()
        for size, when, name in sorted(mine, key=lambda r: -r[0])[:12]:
            ts = time.strftime("%m-%d %H:%M", time.localtime(when)) if when else "?"
            p = name if len(name) <= 52 else f"...{name[-49:]}"
            print(f"  {ts:<12}{hsize(size):>11}  {p}")
        if len(mine) > 12:
            print(f"  … 另有 {len(mine) - 12:,} 条")

    print()
    print("说明：只读取了 $I 元数据，没有触碰 $R 里的数据本体，")
    print("      也没有修改回收站任何内容。")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
