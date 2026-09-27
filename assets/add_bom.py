"""给启动器脚本补 UTF-8 BOM。

为什么必须做：Windows 自带的 5.1 版把「无 BOM 的 UTF-8」按系统 ANSI 代码页
（简体中文环境是 GBK）解析，脚本里的中文会全部变成乱码 —— 界面文字和路径
都读不出来。加了 BOM 它才认 UTF-8。

用法：python assets/add_bom.py <文件> [<文件> ...]
"""

from __future__ import annotations

import sys
from pathlib import Path

BOM = b"\xef\xbb\xbf"


def main(paths: list[str]) -> int:
    rc = 0
    for a in paths:
        p = Path(a)
        if not p.exists():
            print(f"  跳过（不存在）: {p}")
            rc = 1
            continue
        raw = p.read_bytes()
        if raw.startswith(BOM):
            print(f"  已有 BOM: {p.name}")
            continue
        p.write_bytes(BOM + raw)
        print(f"  已加 BOM: {p.name}  ({len(raw)} → {p.stat().st_size} 字节)")

        # 校验：确认解码后中文完好
        text = p.read_text(encoding="utf-8-sig")
        sample = [ln for ln in text.splitlines() if "磁盘内容盘点" in ln][:1]
        if sample:
            print(f"    中文抽样: {sample[0].strip()[:40]}")
        else:
            print("    ⚠ 未找到预期的中文抽样，请检查内容")
    return rc


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1:]))
