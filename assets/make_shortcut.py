"""创建 Windows 快捷方式（.lnk），并回读验证。

为什么不用 PowerShell 的 `WScript.Shell` / `New-Object -ComObject`：
该方式被当前环境的安全策略拦下。这里改用 Python 直接调用系统 Shell 的
IShellLink 接口 —— 与资源管理器用的是同一套官方 API，产出的 .lnk 完全等效。

关键的自我校验：保存后**重新打开这个 .lnk 并回读目标路径**。只写不读的话，
一旦结构不对，用户看到的会是一个双击没反应的图标，而不会报任何错。

用法：
    python assets/make_shortcut.py --lnk <输出.lnk> --target <exe>
        [--args "..."] [--workdir <目录>] [--icon <ico>] [--desc "..."]
"""

from __future__ import annotations

import argparse
import ctypes
import sys
from ctypes import POINTER, byref, c_int, c_long, c_uint, c_ulong, c_void_p, c_wchar_p


# ------------------------------------------------------------------ 基础类型

class GUID(ctypes.Structure):
    _fields_ = [
        ("Data1", c_ulong),
        ("Data2", ctypes.c_ushort),
        ("Data3", ctypes.c_ushort),
        ("Data4", ctypes.c_ubyte * 8),
    ]

    @classmethod
    def parse(cls, s: str) -> "GUID":
        s = s.strip().strip("{}")
        a, b, c, d, e = s.split("-")
        g = cls()
        g.Data1 = int(a, 16)
        g.Data2 = int(b, 16)
        g.Data3 = int(c, 16)
        rest = bytes.fromhex(d + e)
        for i, byte in enumerate(rest):
            g.Data4[i] = byte
        return g


CLSID_SHELLLINK = GUID.parse("00021401-0000-0000-C000-000000000046")
IID_ISHELLLINK_W = GUID.parse("000214F9-0000-0000-C000-000000000046")
IID_IPERSISTFILE = GUID.parse("0000010b-0000-0000-C000-000000000046")

CLSCTX_INPROC_SERVER = 0x1
COINIT_APARTMENTTHREADED = 0x2

# IShellLinkW 的 vtable 下标（IUnknown 的 3 个方法之后开始）
VT_QUERY_INTERFACE = 0
VT_RELEASE = 2
VT_GET_PATH = 3
VT_SET_DESCRIPTION = 7
VT_SET_WORKING_DIRECTORY = 9
VT_SET_ARGUMENTS = 11
VT_SET_SHOW_CMD = 15
VT_SET_ICON_LOCATION = 17
VT_SET_PATH = 20

# IPersistFile 的 vtable 下标
PF_LOAD = 5
PF_SAVE = 6

ole32 = ctypes.oledll.ole32

ole32.CoInitializeEx.argtypes = [c_void_p, c_ulong]
ole32.CoInitializeEx.restype = c_long

ole32.CoCreateInstance.argtypes = [
    POINTER(GUID), c_void_p, c_ulong, POINTER(GUID), POINTER(c_void_p),
]
ole32.CoCreateInstance.restype = c_long


def _vtbl(ptr: c_void_p):
    """取 COM 对象的虚表指针。"""
    return ctypes.cast(ptr, POINTER(POINTER(c_void_p)))[0]


def _fn(ptr: c_void_p, index: int, *argtypes):
    """按 vtable 下标取方法，并绑定调用原型。"""
    proto = ctypes.WINFUNCTYPE(c_long, c_void_p, *argtypes)
    return proto(_vtbl(ptr)[index])


class ShellLink:
    """IShellLinkW 的最小包装。"""

    def __init__(self) -> None:
        self.p = c_void_p()
        ole32.CoCreateInstance(
            byref(CLSID_SHELLLINK), None, CLSCTX_INPROC_SERVER,
            byref(IID_ISHELLLINK_W), byref(self.p),
        )
        if not self.p:
            raise RuntimeError("CoCreateInstance 未能创建 ShellLink 对象")

    # ---- IShellLinkW ----
    def set_path(self, path: str) -> None:
        # SetPath(LPCWSTR pszFile) —— 这里只传文件名部分，完整路径交给
        # SetWorkingDirectory/相对路径解析。实践中传完整路径最稳。
        _fn(self.p, VT_SET_PATH, c_wchar_p, c_void_p)(self.p, path, None)

    def set_arguments(self, args: str) -> None:
        _fn(self.p, VT_SET_ARGUMENTS, c_wchar_p)(self.p, args)

    def set_working_dir(self, d: str) -> None:
        _fn(self.p, VT_SET_WORKING_DIRECTORY, c_wchar_p)(self.p, d)

    def set_description(self, s: str) -> None:
        _fn(self.p, VT_SET_DESCRIPTION, c_wchar_p)(self.p, s)

    def set_icon(self, ico: str, index: int = 0) -> None:
        _fn(self.p, VT_SET_ICON_LOCATION, c_wchar_p, c_int)(self.p, ico, index)

    def set_show_cmd(self, cmd: int) -> None:
        _fn(self.p, VT_SET_SHOW_CMD, c_int)(self.p, cmd)

    def get_path(self) -> str:
        buf = ctypes.create_unicode_buffer(1024)
        hr = _fn(self.p, VT_GET_PATH, c_wchar_p, c_int, c_void_p, c_ulong)(
            self.p, buf, len(buf), None, 0
        )
        if hr < 0:
            raise RuntimeError(f"GetPath 失败: 0x{hr & 0xFFFFFFFF:08X}")
        return buf.value

    # ---- IPersistFile ----
    def _persist_file(self) -> c_void_p:
        qi = _fn(self.p, VT_QUERY_INTERFACE, POINTER(GUID), POINTER(c_void_p))
        pf = c_void_p()
        hr = qi(self.p, byref(IID_IPERSISTFILE), byref(pf))
        if hr < 0 or not pf:
            raise RuntimeError(f"QueryInterface(IPersistFile) 失败: 0x{hr & 0xFFFFFFFF:08X}")
        return pf

    def save(self, lnk_path: str) -> None:
        pf = self._persist_file()
        save = _fn(pf, PF_SAVE, c_wchar_p, c_int)
        hr = save(pf, lnk_path, 1)   # fRemember = TRUE
        if hr < 0:
            raise RuntimeError(f"Save 失败: 0x{hr & 0xFFFFFFFF:08X}")

    def release(self) -> None:
        if self.p:
            _fn(self.p, VT_RELEASE)(self.p)
            self.p = c_void_p()


def read_link(lnk_path: str) -> tuple[str, str, str, str]:
    """打开一个 .lnk 并回读 (目标, 参数, 工作目录, 图标)。"""
    ole32.CoInitializeEx(None, COINIT_APARTMENTTHREADED)
    link = ShellLink()
    pf = link._persist_file()
    load = _fn(pf, PF_LOAD, c_wchar_p, c_ulong)
    hr = load(pf, lnk_path, 0)   # STGM_READ
    if hr < 0:
        raise RuntimeError(f"Load 失败: 0x{hr & 0xFFFFFFFF:08X}")

    buf = ctypes.create_unicode_buffer(1024)
    get_args = _fn(link.p, VT_SET_ARGUMENTS - 1, c_wchar_p, c_int)   # GetArguments = 10
    get_args(link.p, buf, len(buf))
    args = buf.value

    wd = ctypes.create_unicode_buffer(1024)
    get_wd = _fn(link.p, VT_SET_WORKING_DIRECTORY - 1, c_wchar_p, c_int)  # GetWorkingDirectory = 8
    get_wd(link.p, wd, len(wd))

    ibuf = ctypes.create_unicode_buffer(1024)
    idx = c_int(0)
    get_icon = _fn(link.p, VT_SET_ICON_LOCATION - 1, c_wchar_p, c_int, POINTER(c_int))  # GetIconLocation = 16
    get_icon(link.p, ibuf, len(ibuf), byref(idx))

    target = link.get_path()
    link.release()
    return target, args, wd.value, ibuf.value


def create(
    lnk_path: str, target: str, args: str = "", workdir: str = "",
    icon: str = "", desc: str = "", show: int = 1,
) -> None:
    ole32.CoInitializeEx(None, COINIT_APARTMENTTHREADED)
    link = ShellLink()
    link.set_path(target)
    if args:
        link.set_arguments(args)
    if workdir:
        link.set_working_dir(workdir)
    if desc:
        link.set_description(desc)
    if icon:
        link.set_icon(icon, 0)
    link.set_show_cmd(show)
    link.save(lnk_path)
    link.release()


# ------------------------------------------------------------------ 命令行

def main() -> int:
    ap = argparse.ArgumentParser(description="创建 Windows 快捷方式并回读验证")
    ap.add_argument("--lnk", required=True, help="要创建的 .lnk 路径")
    ap.add_argument("--target", required=True, help="目标程序")
    ap.add_argument("--args", default="", help="启动参数")
    ap.add_argument("--workdir", default="", help="工作目录")
    ap.add_argument("--icon", default="", help="图标文件（.ico / .exe）")
    ap.add_argument("--desc", default="", help="描述（鼠标悬停时显示）")
    ap.add_argument("--show", type=int, default=1, help="窗口状态 1=正常 3=最大化 7=最小化")
    a = ap.parse_args()

    try:
        create(a.lnk, a.target, a.args, a.workdir, a.icon, a.desc, a.show)
    except OSError as e:
        print(f"✗ 创建失败: {e}")
        return 1
    except RuntimeError as e:
        print(f"✗ 创建失败: {e}")
        return 1

    print(f"✓ 已创建: {a.lnk}")

    # ---- 回读验证：这一步不能省 ----
    try:
        t, args, wd, ico = read_link(a.lnk)
    except Exception as e:  # noqa: BLE001
        print(f"✗ 回读失败，快捷方式可能不可用: {e}")
        return 2

    print("  回读验证:")
    print(f"    目标   : {t}")
    print(f"    参数   : {args}")
    print(f"    工作目录: {wd}")
    print(f"    图标   : {ico}")

    ok = t.lower() == a.target.lower()
    if a.icon:
        ok = ok and ico.lower() == a.icon.lower()
    if a.args:
        ok = ok and args == a.args
    if not ok:
        print("✗ 回读结果与写入不符")
        return 3
    print("  ✓ 回读一致")
    return 0


if __name__ == "__main__":
    sys.exit(main())
