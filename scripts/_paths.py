"""项目内路径解析 —— 供 scripts/ 下的检查脚本共用。

## 为什么需要这个模块

这些脚本原本各自硬编码了绝对路径（例如 `C:\\Users\\<用户名>\\<某个工作区>\\...`）。
项目一搬到别的目录，**全部失效**，而且失败方式是"找不到文件"，
看不出是路径写死了。

集中一处解析 + 自动探测，脚本才真正可迁移。

## 探测策略

不假设任何东西一定在哪：

  · 项目根目录    → 以本文件的上级目录为准（跟着项目走）
  · 构建产物      → 优先项目内 `target/`，没有就退到已安装的位置
  · Node          → 环境变量 → 常见安装位置 → PATH
  · 测试用的沙盒  → 项目内 `.tmp/`（不污染用户目录）

任何一项都允许用环境变量覆盖，方便在别的机器上跑。
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
from pathlib import Path

# ------------------------------------------------------------------ 基本路径

ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = ROOT / "scripts"
REPORTS = ROOT / "reports"
DOCS = ROOT / "docs"
ASSETS = ROOT / "assets"

# 源码里的两个关键文件（检查脚本要读它们）
RULES_YAML = ROOT / "crates" / "dd-rules" / "rules" / "rules.yaml"
SURVEY_RS = ROOT / "crates" / "dd-cli" / "src" / "survey.rs"

# 界面与数据（由 survey 命令生成，存放在 reports/）
SURVEY_HTML = REPORTS / "我的磁盘内容.html"
SURVEY_JSON = REPORTS / "my-disk.json"


def check_sources() -> list[str]:
    """自检源码结构是否完整。返回问题列表（空表示没问题）。

    存在的理由：这几个检查脚本都依赖源码里的具体文件。**目录结构一旦变动，
    失败方式只是"找不到文件"**，看不出是路径写错了还是文件真的没了。
    集中在这里检查一次，就能给出明确的原因。
    """
    problems = []
    for label, p in [
        ("规则库", RULES_YAML),
        ("界面源码", SURVEY_RS),
    ]:
        if not p.exists():
            problems.append(f"{label}不存在: {p}")
    if not (ROOT / "crates").is_dir():
        problems.append(f"crates/ 目录不存在: {ROOT / 'crates'}")
    if not (ROOT / "Cargo.toml").exists():
        problems.append(f"Cargo.toml 不存在: {ROOT / 'Cargo.toml'}")
    return problems


def survey_html() -> Path:
    """界面文件位置。允许 DD_SURVEY_HTML 覆盖。"""
    p = os.environ.get("DD_SURVEY_HTML")
    return Path(p) if p else SURVEY_HTML


# ------------------------------------------------------------------ 可执行文件

# 安装位置（由 assets/install.py 部署）
INSTALLED_DIR = (
    Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    / "Programs" / "DiskDoctor"
)


def diskdoctor_exe() -> Path | None:
    """找到可用的 diskdoctor 可执行文件。

    优先项目内刚构建的，其次已安装的那份 —— 这样**没构建过也能跑检查**，
    否则迁移完第一件事就是"所有脚本报找不到 exe"。
    """
    override = os.environ.get("DD_EXE")
    if override:
        p = Path(override)
        return p if p.exists() else None

    candidates = [
        ROOT / "target" / "release" / "diskdoctor.exe",
        ROOT / "target" / "debug" / "diskdoctor.exe",
        INSTALLED_DIR / "diskdoctor.exe",
    ]
    for c in candidates:
        if c.exists():
            return c
    # PATH 里也可能有
    found = shutil.which("diskdoctor") or shutil.which("diskdoctor.exe")
    return Path(found) if found else None


def require_exe() -> Path:
    p = diskdoctor_exe()
    if p is None:
        print("✗ 找不到 diskdoctor 可执行文件。")
        print("  请先构建：cargo build --release")
        print("  或安装：python assets/install.py")
        print("  或指定：set DD_EXE=<路径>")
        sys.exit(2)
    return p


# ------------------------------------------------------------------ Node

def node_exe() -> Path | None:
    """找到 node。UI 检查脚本需要它做真实的语法/运行时验证。"""
    override = os.environ.get("DD_NODE")
    if override:
        p = Path(override)
        return p if p.exists() else None

    if found := shutil.which("node"):
        return Path(found)

    home = Path.home()
    # 常见安装位置（含 WorkBuddy 自带的托管版本）
    patterns = [
        home / ".workbuddy" / "binaries" / "node" / "versions",
        Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "nodejs",
        Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "nodejs",
    ]
    for base in patterns:
        if not base.exists():
            continue
        if (base / "node.exe").exists():
            return base / "node.exe"
        # 版本化目录：取版本号最大的那个
        subs = sorted(
            (d for d in base.iterdir() if d.is_dir() and (d / "node.exe").exists()),
            key=lambda d: d.name, reverse=True,
        )
        if subs:
            return subs[0] / "node.exe"
    return None


def require_node() -> Path:
    p = node_exe()
    if p is None:
        print("✗ 找不到 node。")
        print("  请安装 node，或 set DD_NODE=<node.exe 路径>")
        sys.exit(2)
    return p


# ------------------------------------------------------------------ 临时工作区

def work_dir(name: str) -> Path:
    """给检查脚本用的临时目录。

    放在项目内而不是用户目录（`C:\\Users\\...`）——
    测试沙盒没有理由污染用户的家目录，清理时也不会漏。
    """
    base = Path(os.environ.get("DD_TMP") or (ROOT / ".tmp"))
    d = base / name
    d.mkdir(parents=True, exist_ok=True)
    return d


def system_tmp() -> Path:
    return Path(tempfile.gettempdir())


# ------------------------------------------------------------------ 自检

def describe() -> str:
    exe = diskdoctor_exe()
    node = node_exe()
    problems = check_sources()
    lines = [
        f"项目根目录 : {ROOT}",
        f"源码结构   : {'✓ 完整' if not problems else '✗ 有问题'}",
    ]
    for p in problems:
        lines.append(f"    ✗ {p}")
    lines += [
        f"规则库     : {RULES_YAML}  (存在={RULES_YAML.exists()})",
        f"界面源码   : {SURVEY_RS}  (存在={SURVEY_RS.exists()})",
        f"报告目录   : {REPORTS}  (存在={REPORTS.is_dir()})",
        f"界面文件   : {survey_html()}  (存在={survey_html().exists()})",
        f"可执行文件 : {exe}" if exe else "可执行文件 : ✗ 未找到",
        f"Node       : {node}" if node else "Node       : ✗ 未找到",
        f"临时目录   : {ROOT / '.tmp'}",
    ]
    return "\n".join(lines)


if __name__ == "__main__":
    print("=== 路径解析自检 ===")
    print(describe())
