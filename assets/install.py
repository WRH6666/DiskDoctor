"""DiskDoctor 安装 / 卸载脚本。

做三件事：
  1. 把 diskdoctor.exe / 图标 / 启动器装到用户级程序目录（不需要管理员）
  2. 在桌面和开始菜单创建快捷方式，并**回读验证**
  3. 卸载时**送进回收站**而不是直接删

为什么装在 `%LOCALAPPDATA%\\Programs\\DiskDoctor`：
这是 Windows 上用户级程序的规范位置（VS Code、Cursor 这类"仅为我安装"
的程序都在这里）。放这里就不需要管理员权限，也不需要动 Program Files。

用法：
    python assets/install.py               # 安装（默认位置）
    python assets/install.py --no-shortcut # 只装文件，不建快捷方式
    python assets/install.py --uninstall   # 卸载（送回收站）
"""

from __future__ import annotations

import argparse
import ctypes
import os
import shutil
import sys
import time
from ctypes import c_int, c_uint, c_void_p
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from make_shortcut import create, read_link  # noqa: E402

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parent

APP_NAME = "DiskDoctor"
DEFAULT_DIR = Path(os.environ["LOCALAPPDATA"]) / "Programs" / APP_NAME

DESCRIPTION = "磁盘内容盘点 —— 看清每个目录是什么、能不能删；勾选后清理，可撤销"

# 要安装的文件：(源, 目标文件名)
PAYLOAD = [
    (PROJECT / "target" / "release" / "diskdoctor.exe", "diskdoctor.exe"),
    (HERE / "diskdoctor.ico", "diskdoctor.ico"),
    (HERE / "DiskDoctor.ps1", "DiskDoctor.ps1"),
]

# 覆盖安装时需要保留的用户数据 —— 卸载时不动它
USER_DATA_DIR = Path(os.environ["LOCALAPPDATA"]) / APP_NAME


# ------------------------------------------------------- 系统特殊目录

# 桌面 / 开始菜单这类路径**不能靠拼接猜**：用户可能把它们重定向到别的盘
# （OneDrive 接管、或手工改过位置）。本机上桌面就指向 `D:\桌面`，
# 而 `%USERPROFILE%\Desktop` 只是个过时的空壳目录 —— 建在那里用户看不到。
# 正确做法是问系统，`SHGetFolderPathW` 读的就是资源管理器在用的那个值。
CSIDL_DESKTOPDIRECTORY = 0x0010
CSIDL_COMMON_DESKTOPDIRECTORY = 0x0019
CSIDL_PROGRAMS = 0x0002            # 当前用户的「开始菜单 → 程序」
CSIDL_COMMON_PROGRAMS = 0x0017     # 所有用户的「开始菜单 → 程序」


def shell_folder(csidl: int) -> Path | None:
    """按 CSIDL 取系统特殊目录。失败返回 None。"""
    buf = ctypes.create_unicode_buffer(1024)
    rc = ctypes.windll.shell32.SHGetFolderPathW(None, csidl, None, 0, buf)
    if rc != 0 or not buf.value:
        return None
    p = Path(buf.value)
    return p if p.is_dir() else None


def resolve_link_targets() -> tuple[list[Path], Path | None]:
    """找出该把快捷方式放哪。

    返回 (目标位置列表, 用户桌面路径)。桌面路径单独返回，用于精确报告 ——
    因为"桌面被重定向"这件事必须让用户知道，否则他会以为没建成功。
    """
    places: list[Path] = []

    desktop = shell_folder(CSIDL_DESKTOPDIRECTORY)
    if desktop:
        places.append(desktop / f"{APP_NAME}.lnk")

    progs = shell_folder(CSIDL_PROGRAMS)
    if progs:
        places.append(progs / f"{APP_NAME}.lnk")

    return places, desktop


def legacy_desktop() -> Path | None:
    """`%USERPROFILE%\\Desktop` —— 仅用于检测"这里有个过时目录"。

    它可能和真实的桌面**不是同一个目录**。如果它存在且与真实桌面不同，
    说明用户改过桌面位置，那个目录里的东西已经不在桌面上了。
    """
    p = Path(os.environ["USERPROFILE"]) / "Desktop"
    return p if p.is_dir() else None


# ------------------------------------------------------- 送回收站

class SHFILEOPSTRUCTW(ctypes.Structure):
    # `pFrom` / `pTo` 声明成 c_void_p，而不是 c_wchar_p：
    # pFrom 是**双 null 结尾**的字符串列表。用 c_wchar_p 时会踩到一个坑 ——
    # ctypes 按"一个字符串"来编组，字符串里内嵌的 NUL 让它变成
    # "第一个元素 + 空元素"，结尾标记失效。表现很隐蔽：
    # API 返回 ERROR_FILE_NOT_FOUND(2)，但第一项其实已经被删了。
    # 用显式缓冲区 + 传地址，语义就没有歧义。
    _fields_ = [
        ("hwnd", c_void_p),
        ("wFunc", c_uint),
        ("pFrom", c_void_p),
        ("pTo", c_void_p),
        ("fFlags", ctypes.c_ushort),
        ("fAnyOperationsAborted", c_int),
        ("hNameMappings", c_void_p),
        ("lpszProgressTitle", c_void_p),
    ]


FO_DELETE = 0x0003
FOF_ALLOWUNDO = 0x0040      # 进回收站（这就是"可撤销"的来源）
FOF_NOCONFIRMATION = 0x0010
FOF_SILENT = 0x0004
FOF_NOERRORUI = 0x0400

_SHCODES = {
    2: "找不到指定文件",
    3: "找不到指定路径",
    5: "访问被拒绝",
    32: "文件正被其他程序占用",
    0x71: "路径过长",
    0x7C: "路径无效",
}


def build_path_list(paths: list[Path]):
    """构造双 null 结尾的宽字符列表。返回 None 表示没有有效项。"""
    parts = [str(p) for p in paths if p.exists()]
    if not parts:
        return None
    s = "".join(p + "\x00" for p in parts) + "\x00"
    # create_unicode_buffer 逐字符拷入（含内嵌 NUL），并自动补结尾 NUL，
    # 正好构成 API 需要的"双 null 结尾"。
    return ctypes.create_unicode_buffer(s, len(s) + 1)


def send_to_recycle_bin(paths: list[Path]) -> tuple[bool, str]:
    """把文件/目录送进回收站。

    刻意不用 `rmtree`/`os.remove`：那不可逆。我们的产品自己主张"删除 = 移动
    到可撤销的地方"，卸载自己的组件时当然也该守这条规矩。
    """
    buf = build_path_list(paths)
    if buf is None:
        return True, "无内容"

    op = SHFILEOPSTRUCTW()
    op.wFunc = FO_DELETE
    op.pFrom = ctypes.addressof(buf)     # 保持对 buf 的引用，别被 GC 回收
    op.pTo = None
    op.fFlags = FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI

    res = ctypes.windll.shell32.SHFileOperationW(ctypes.byref(op))
    del buf
    if res != 0:
        return False, f"SHFileOperationW 返回 {res}（{_SHCODES.get(res, '未知')}）"
    if op.fAnyOperationsAborted:
        return False, "操作被中断（可能有文件被占用）"
    return True, "已送入回收站"


# ------------------------------------------------------- 安装

def create_link_with_retry(lnk: Path, **kw) -> tuple[bool, str]:
    """创建快捷方式，带重试与降级。

    为什么要重试：.lnk 刚被创建之后，文件索引器或杀毒软件可能短暂持有它，
    此时覆盖会返回 E_ACCESSDENIED（0x80070005）。实测遇到过 —— 同一路径、
    同一方法，几秒后重试就成功。对用户来说"装到一半失败"不可接受，
    所以这里退避重试；仍然失败就降级为"先删旧的、再建新的"。

    返回 (是否成功, 说明)。
    """
    last = ""
    for attempt in range(4):
        try:
            create(str(lnk), **kw)
            return True, ""
        except (OSError, RuntimeError) as e:  # noqa: PERF203
            last = str(e)
            time.sleep(0.4 * (attempt + 1))

    # 降级：把旧的送进回收站，再建新的。
    # 用回收站而不是硬删 —— 和我们给用户的主张保持一致。
    try:
        if lnk.exists():
            send_to_recycle_bin([lnk])
        create(str(lnk), **kw)
        return True, "（旧的已送回收站后重建）"
    except (OSError, RuntimeError) as e:
        return False, f"{last}；降级后仍失败: {e}"


def existing_link_is_correct(lnk: Path, target: str, args: str, icon: str) -> bool:
    """已存在的快捷方式内容是否已经是我们想要的。

    有些环境会拦下"覆盖已有文件"（见 create_link_with_retry 的说明）。
    此时不该直接报失败 —— **先看看现有的那份是不是本来就对**。
    内容对的话报 ✗ 会让人以为装坏了，跑去手工折腾一个并没坏的东西。
    """
    if not lnk.exists():
        return False
    try:
        t, a, _wd, i = read_link(str(lnk))
    except Exception:  # noqa: BLE001
        return False
    return t.lower() == target.lower() and a == args and i.lower() == icon.lower()


def install(target_dir: Path, with_shortcut: bool) -> int:
    rc = 0

    # ---- 1. 拷贝文件 ----
    target_dir.mkdir(parents=True, exist_ok=True)
    print(f"安装目录: {target_dir}")
    for src, name in PAYLOAD:
        if not src.exists():
            print(f"  ✗ 缺少源文件: {src}")
            if name == "diskdoctor.exe":
                print("    请先构建: cargo build --release -p dd-cli")
            return 1
        dst = target_dir / name
        shutil.copy2(src, dst)
        print(f"  ✓ {name}  ({dst.stat().st_size / 1024:.0f} KB)")

    # ---- 2. 自检：装出来的 exe 能跑吗 ----
    exe = target_dir / "diskdoctor.exe"
    probe = os.popen(f'"{exe}" --help 2>&1').read()
    if "DiskDoctor" not in probe:
        print("  ✗ 装好的 exe 无法运行")
        return 1
    print("  ✓ exe 可运行")

    # 启动器语法自检（没装 python 的机器上跑不了这步，但这里是开发机）
    try:
        ps1 = target_dir / "DiskDoctor.ps1"
        raw = ps1.read_bytes()
        if not raw.startswith(b"\xef\xbb\xbf"):
            print("  ⚠ 启动器缺少 UTF-8 BOM，中文会乱码")
            rc = 2
        else:
            print("  ✓ 启动器编码正确")
    except OSError:
        pass

    if not with_shortcut:
        return rc

    # ---- 3. 快捷方式 ----
    ps_exe = Path(os.environ["SystemRoot"]) / "System32" / "WindowsPowerShell" / "v1.0" / "powershell.exe"
    if not ps_exe.exists():
        print(f"  ✗ 找不到 PowerShell: {ps_exe}")
        return 1

    ps1_path = target_dir / "DiskDoctor.ps1"
    ico_path = target_dir / "diskdoctor.ico"
    lnk_args = f'-NoLogo -NoProfile -ExecutionPolicy Bypass -File "{ps1_path}"'

    # 桌上的位置要问系统，不能拼路径 —— 见文件上方 shell_folder 的说明
    places, desktop = resolve_link_targets()
    print(f"  桌面位置 : {desktop}" if desktop else "  ⚠ 取不到桌面路径")
    progs = shell_folder(CSIDL_PROGRAMS)
    print(f"  开始菜单 : {progs}" if progs else "  ⚠ 取不到开始菜单路径")

    # 提醒：如果 %USERPROFILE%\Desktop 与真实桌面不是同一个目录，
    # 说明用户改过桌面位置，那个目录里的东西**已经不在桌面上显示了**。
    legacy = legacy_desktop()
    if legacy and desktop and legacy.resolve() != desktop.resolve():
        print()
        print(f"  注：{legacy} 与真实桌面不是同一目录（你改过桌面位置）。")
        print("      那里放的东西不会显示在桌面上 —— 本脚本只写真实桌面。")

    if not places:
        print("  ✗ 桌面与开始菜单都不可用")
        return 1

    for lnk in places:
        ico_path_s = str(target_dir / "diskdoctor.ico")
        ok, note = create_link_with_retry(
            lnk,
            target=str(ps_exe),
            args=lnk_args,
            workdir=str(target_dir),
            icon=ico_path_s,
            desc=DESCRIPTION,
            show=1,
        )
        if not ok:
            # 覆盖被拦时先查现有内容 —— 本来就对的话不该报失败
            if existing_link_is_correct(lnk, str(ps_exe), lnk_args, ico_path_s):
                print(f"  ✓ {lnk}")
                print("      （无法覆盖，但已存在的那份内容是正确的 —— 无需处理）")
                continue
            print(f"  ✗ 创建失败 {lnk}: {note}")
            if "拒绝访问" in note or "0x80070005" in note:
                print("      原因：覆盖或删除该文件的操作被系统/安全软件拦下了。")
                print("            （换个文件名新建不受影响 —— 说明拦截点在「移除已有文件」这一步）")
                print("      处理：手工删掉这个快捷方式，再重新运行本脚本。")
            rc = 1
            continue

        # 回读验证 —— 只写不读的话，坏的快捷方式不会报错，只会双击没反应
        try:
            t, a, wd, ico = read_link(str(lnk))
        except Exception as e:  # noqa: BLE001
            print(f"  ✗ 回读失败 {lnk.name}: {e}")
            rc = 1
            continue

        good = (
            t.lower() == str(ps_exe).lower()
            and a == lnk_args
            and ico.lower() == ico_path_s.lower()
        )
        mark = "✓" if good else "✗"
        print(f"  {mark} {lnk}{note}")
        if not good:
            print(f"      目标: {t}")
            print(f"      参数: {a}")
            rc = 1

    print()
    print("安装完成。双击桌面上的 DiskDoctor 即可使用。")
    return rc


# ------------------------------------------------------- 卸载

def uninstall(target_dir: Path) -> int:
    print("卸载 —— 所有内容会**送进回收站**，不是直接删除。")
    print()

    victims: list[Path] = []

    # 走和安装同一套路径解析，否则会漏掉被重定向出去的桌面
    places, _ = resolve_link_targets()
    for lnk in places:
        if lnk.exists():
            victims.append(lnk)

    # 顺带清理早期版本误建在 %USERPROFILE%\Desktop 的那一份（若有）
    legacy = legacy_desktop()
    if legacy:
        stale = legacy / f"{APP_NAME}.lnk"
        if stale.exists() and stale not in victims:
            victims.append(stale)

    if target_dir.exists():
        victims.append(target_dir)

    if not victims:
        print("  没有找到已安装的内容。")
        return 0

    for v in victims:
        print(f"  · {v}")
    print()

    ok, msg = send_to_recycle_bin(victims)
    if ok:
        print(f"  ✓ {msg}（可从回收站还原）")
        if USER_DATA_DIR.exists():
            print()
            print(f"  提示：你的扫描结果保留在 {USER_DATA_DIR}\\out")
            print("        要一并清掉的话自己删即可 —— 它不包含程序文件。")
        return 0

    print(f"  ✗ 卸载失败: {msg}")
    return 1


# ------------------------------------------------------- 入口

def main() -> int:
    ap = argparse.ArgumentParser(description="DiskDoctor 安装 / 卸载")
    ap.add_argument("--dir", default=str(DEFAULT_DIR), help="安装位置")
    ap.add_argument("--no-shortcut", action="store_true", help="只装文件，不建快捷方式")
    ap.add_argument("--uninstall", action="store_true", help="卸载（送回收站）")
    a = ap.parse_args()

    target = Path(a.dir)
    if a.uninstall:
        return uninstall(target)
    return install(target, not a.no_shortcut)


if __name__ == "__main__":
    sys.exit(main())
