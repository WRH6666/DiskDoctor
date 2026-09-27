"""本机服务的回归测试：安全边界 + 健康检查 + 空闲退出。

这套东西是"网页能调用本机进程"的通道，必须按不可信输入来防，
所以每一条"必须拒绝"的情形都要有用例。

**两个容易踩的坑（都踩过）**

1. `urllib` 会读 `http_proxy` / `HTTP_PROXY` 环境变量，**连 127.0.0.1 的
   请求也会发给代理**，于是拿到 502 —— 测的是代理，不是服务。
   这里显式用 `ProxyHandler({})` 直连。
   （浏览器不受影响：它读的是系统代理设置，绕过列表里通常含 `127.*`。）

2. 服务会**主动退出**（空闲超时）。所以"测试失败"也可能是服务已经停了 ——
   报错时要能区分。

用法：
    python scripts/check_serve.py
"""

from __future__ import annotations

import os
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
LAB = _paths.work_dir("serve-lab")
HTML = _paths.work_dir("serve-lab") / "serve.html"

# 显式直连，绕过环境里的代理
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def setup_lab() -> None:
    (LAB / "sub").mkdir(parents=True, exist_ok=True)
    (LAB / "sub" / "a.txt").write_bytes(b"x" * 128)
    (LAB / "sub2").mkdir(parents=True, exist_ok=True)
    (LAB / "sub2" / "b.txt").write_bytes(b"y" * 64)


def cleanup_lab() -> None:
    import shutil
    try:
        shutil.rmtree(LAB)
    except OSError:
        pass


class Service:
    def __init__(self, idle_secs: int | None = None, root: Path = LAB):
        env = dict(os.environ)
        if idle_secs is not None:
            env["DD_SERVE_IDLE_SECS"] = str(idle_secs)
        if HTML.exists():
            HTML.unlink()
        self.proc = subprocess.Popen(
            [str(EXE), "survey", str(root), "--html", str(HTML),
             "--serve", "--no-mft", "--merge-below-kb", "0"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", errors="replace", env=env,
        )
        self.port = 0
        self.token = ""
        for _ in range(150):
            if HTML.exists():
                m = re.search(
                    r'const SERVER = \{"port":(\d+),"token":"([0-9a-f]+)"\}',
                    HTML.read_text(encoding="utf-8", errors="replace"))
                if m:
                    self.port, self.token = int(m.group(1)), m.group(2)
                    return
            if self.proc.poll() is not None:
                raise RuntimeError("服务进程提前退出:\n" + (self.proc.stdout.read() or "")[-1200:])
            time.sleep(0.3)
        raise RuntimeError("未能从界面取到服务信息")

    def call(self, action: str, path: str | None = None,
             token: str | None = None, host: str | None = None,
             method: str = "GET") -> tuple[int | str, bytes]:
        qs = f"/act?a={urllib.parse.quote(action)}"
        qs += f"&t={urllib.parse.quote(self.token if token is None else token)}"
        if path is not None:
            qs += f"&p={urllib.parse.quote(str(path))}"
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{qs}", method=method)
        if host:
            req.add_header("Host", host)
        try:
            with OPENER.open(req, timeout=8) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except Exception as e:  # noqa: BLE001
            return f"失败({type(e).__name__})", b""

    def stop(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def main() -> int:
    if not EXE.exists():
        print(f"✗ 找不到 {EXE}（先 cargo build --release -p dd-cli）")
        return 1

    setup_lab()
    results: list[bool] = []

    def check(name: str, got, want, note: str = "") -> None:
        ok = got == want
        results.append(ok)
        print(f"  {'✓' if ok else '✗'} {name:<28} {got}"
              + (f"  {note}" if note else "")
              + ("" if ok else f"   （期望 {want}）"))

    try:
        print("=== 启动服务 ===")
        svc = Service()
        print(f"  端口 {svc.port}，令牌 {svc.token[:10]}…（{len(svc.token)} 位）")

        print()
        print("=" * 68)
        print("安全边界：必须拒绝")
        print("=" * 68)
        check("伪造令牌", svc.call("open", LAB, token="0" * 64)[0], 403)
        check("缺少令牌", svc.call("open", LAB, token="")[0], 403)
        check("根外路径 C:\\Windows", svc.call("open", r"C:\Windows")[0], 403)
        check("用 .. 逃逸", svc.call("open", LAB / ".." / ".." / "Windows")[0], 403)
        check("名称前缀相近的兄弟目录", svc.call("open", str(LAB) + "X")[0], 403,
              "（C:\\labX 不是 C:\\lab 的子路径）")
        check("不存在的路径", svc.call("open", LAB / "no-such-xyz")[0], 403)
        check("Host 为外部域名", svc.call("open", LAB, host="evil.example.com")[0], 403)
        check("POST 请求", svc.call("open", LAB, method="POST")[0], 405)
        check("未知动作 delete", svc.call("delete", LAB)[0], 400,
              "（服务只有 open/reveal/ping）")
        check("路径为空", svc.call("open", "")[0], 403)

        print()
        print("=" * 68)
        print("健康检查（前端靠它判断服务是否可达）")
        print("=" * 68)
        code, body = svc.call("ping")
        check("ping 状态码", code, 200)
        check("ping 返回真实图片", body[:6], b"GIF89a",
              "（必须是图片：非图片响应会让 <img> 一律 onerror，"
              "无法区分「服务在」和「不在」）")
        check("ping 不需要路径", svc.call("ping", None)[0], 200)
        check("ping 仍需令牌", svc.call("ping", None, token="0" * 64)[0], 403)

        print()
        print("=" * 68)
        print("正常路径")
        print("=" * 68)
        check("打开扫描根", svc.call("open", LAB)[0], 204)
        check("打开子目录", svc.call("open", LAB / "sub")[0], 204)
        check("定位文件", svc.call("reveal", LAB / "sub" / "a.txt")[0], 204)
        check("路径带引号", svc.call("open", f'"{LAB}"')[0], 204, "（自动清理）")
        check("Host 为 localhost", svc.call("open", LAB, host=f"localhost:{svc.port}")[0], 204)

        svc.stop()
        print()
        print("  服务已停止")

        print()
        print("=" * 68)
        print("空闲自动退出（承诺必须兑现，否则会留孤儿进程）")
        print("=" * 68)
        svc2 = Service(idle_secs=5)
        print(f"  以 5 秒空闲上限重启，端口 {svc2.port}")
        t0 = time.time()
        exited = False
        while time.time() - t0 < 20:
            if svc2.proc.poll() is not None:
                exited = True
                break
            time.sleep(0.4)
        took = time.time() - t0
        results.append(exited)
        print(f"  {'✓' if exited else '✗'} 自行退出: {exited}（耗时 {took:.1f}s）")
        if not exited:
            svc2.stop()

    except Exception as e:  # noqa: BLE001
        print(f"\n✗ 测试过程出错: {e}")
        cleanup_lab()
        return 1
    finally:
        cleanup_lab()

    print()
    print("=" * 68)
    if all(results):
        print(f"✓ 全部 {len(results)} 项通过")
    else:
        print(f"✗ {results.count(False)} / {len(results)} 项失败")
    print("=" * 68)
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
