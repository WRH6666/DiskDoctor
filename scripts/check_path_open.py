"""验证「单击路径 → 直接弹出资源管理器」这条交互。

会**真的弹出资源管理器窗口**（这是验证的目的），跑完自行关闭。

判定方式不是看 HTTP 状态码，而是**枚举系统顶层窗口标题**里是否出现了
目标目录名 —— 返回值 204 只证明"服务接受了请求"，不证明窗口真的开了。
这个项目里反复踩到的教训就是"返回值不等于实际效果"，所以这里用可观测的
副作用来判定。

同时做一条对照：越界路径必须返回 403 **且不打开任何窗口**。

用法：
    python scripts/check_path_open.py
"""

import ctypes
import ctypes.wintypes as wt
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

ROOT = _paths.ROOT
EXE = _paths.require_exe()
# 测试沙盒放项目内，不污染用户的家目录
LAB = _paths.work_dir("click-lab")
HTML = _paths.work_dir("click-lab") / "click.html"

# 造一个结构清晰的小目录
if LAB.exists():
    import shutil
    try:
        shutil.rmtree(LAB)
    except OSError:
        pass
(LAB / "JianyingPro" / "Cache").mkdir(parents=True, exist_ok=True)
(LAB / "JianyingPro" / "Cache" / "big.bin").write_bytes(b"\0" * 2048)
(LAB / "NVIDIA").mkdir(parents=True, exist_ok=True)
(LAB / "NVIDIA" / "driver.dll").write_bytes(b"\0" * 1024)

if HTML.exists():
    HTML.unlink()

# ---------------------------------------------------------------- 枚举窗口

user32 = ctypes.windll.user32
EnumWindowsProc = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)

def top_level_windows():
    """返回当前所有可见顶层窗口的标题。"""
    out = []

    def cb(hwnd, _):
        if user32.IsWindowVisible(hwnd):
            n = user32.GetWindowTextLengthW(hwnd)
            if n > 0:
                buf = ctypes.create_unicode_buffer(n + 1)
                user32.GetWindowTextW(hwnd, buf, n + 1)
                if buf.value:
                    out.append(buf.value)
        return True

    user32.EnumWindows(EnumWindowsProc(cb), 0)
    return out


# 先记录基线，避免把本来就开着的窗口算进来
before = set(top_level_windows())
print(f"基线可见窗口数: {len(before)}")

# ---------------------------------------------------------------- 起服务

print("\n=== 启动 survey --serve ===")
proc = subprocess.Popen(
    [str(EXE), "survey", str(LAB), "--html", str(HTML),
     "--serve", "--no-mft", "--merge-below-kb", "0"],
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    text=True, encoding="utf-8", errors="replace",
)

port = token = None
for _ in range(120):
    if HTML.exists():
        m = re.search(r'const SERVER = \{"port":(\d+),"token":"([0-9a-f]+)"\}',
                      HTML.read_text(encoding="utf-8", errors="replace"))
        if m:
            port, token = int(m.group(1)), m.group(2)
            break
    if proc.poll() is not None:
        print("  ✗ 进程提前退出"); print(proc.stdout.read()[-1200:]); sys.exit(1)
    time.sleep(0.4)

print(f"  ✓ 端口 {port}")

# 确认前端确实是「单击就发请求」
js = HTML.read_text(encoding="utf-8", errors="replace")
click_ok = ("beacon(isDir ? \"open\" : \"reveal\", path)" in js)
print(f"  {'✓' if click_ok else '✗'} 前端在单击处直接调用 beacon（不需要第二次点击）")

results = [click_ok]

def request(action, target):
    qs = (f"/act?a={urllib.parse.quote(action)}"
          f"&t={urllib.parse.quote(token)}"
          f"&p={urllib.parse.quote(str(target))}")
    req = urllib.request.Request(f"http://127.0.0.1:{port}{qs}")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return None


def wait_window_containing(needle, seconds=12):
    """等一个标题里含 needle 的新窗口出现，返回它。"""
    end = time.time() + seconds
    while time.time() < end:
        for t in top_level_windows():
            if needle.lower() in t.lower() and t not in before:
                return t
        time.sleep(0.4)
    return None


print()
print("=" * 70)
print("单击目录 → 应弹出该文件夹")
print("=" * 70)
code = request("open", LAB / "JianyingPro")
print(f"  HTTP 状态: {code}")
results.append(code == 204)

win = wait_window_containing("JianyingPro")
print(f"  新窗口: {win!r}")
results.append(win is not None)
if win:
    print("  ✓ 文件夹窗口确实弹出来了")

print()
print("=" * 70)
print("单击文件 → 应打开所在文件夹并选中它（不是启动文件本身）")
print("=" * 70)
code = request("reveal", LAB / "NVIDIA" / "driver.dll")
print(f"  HTTP 状态: {code}")
results.append(code == 204)

win2 = wait_window_containing("NVIDIA")
print(f"  新窗口: {win2!r}")
results.append(win2 is not None)
if win2:
    print("  ✓ 所在文件夹已打开")

print()
print("=" * 70)
print("对照：越界路径不该打开任何窗口")
print("=" * 70)
before2 = set(top_level_windows())
code = request("open", r"C:\Windows")
print(f"  HTTP 状态: {code}（期望 403）")
results.append(code == 403)
time.sleep(3)
new_wins = set(top_level_windows()) - before2
print(f"  新增窗口数: {len(new_wins)}")
results.append(len(new_wins) == 0)
if new_wins:
    print(f"    ⚠ 意外弹出了: {list(new_wins)[:3]}")

print()
print("=" * 70)
print(f"✓ 全部 {len(results)} 项通过" if all(results)
      else f"✗ {results.count(False)} / {len(results)} 项失败")
print("=" * 70)

print("\n=== 收尾 ===")
proc.terminate()
try:
    proc.wait(timeout=8)
    print("  服务已停止")
except subprocess.TimeoutExpired:
    proc.kill()

print(f"  提示：测试期间弹出的资源管理器窗口（{LAB.name}）可自行关闭")

sys.exit(0 if all(results) else 1)
