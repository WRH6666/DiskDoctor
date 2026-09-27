#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
DiskDoctor 规则库验证器。

Rust 引擎还没法编译，但规则库的正确性可以独立验证 —— 而且这一步
比编译更能说明问题：规则之间互相遮蔽、路径匹配不到，这类错误编译器
一个都抓不出来，但会直接让用户看到错误的安全等级。

它做四件事：
  1. 结构校验：id 唯一、必填字段、模式非空、command 动作必须带命令
  2. 顺序遮蔽检测：更宽泛的规则排在更具体的规则前面 → 后者永远不生效
  3. 匹配语义验证：用一份真实路径样本，检查每条路径落到哪条规则上
  4. 覆盖统计：按类别 / 安全等级汇总

匹配语义刻意和 Rust 端保持一致（含 path-glob 自动补前导 `*`）。
"""

import re
import sys
from pathlib import Path
from collections import defaultdict

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

# 路径由 _paths 统一解析 —— 原本这里写死了 "diskdoctor/crates/..."，
# 项目目录层级一变就找不到规则库
RULES_FILE = _paths.RULES_YAML

REQUIRED = ["id", "name", "match", "category", "safety"]
VALID_KINDS = {"name", "name-glob", "ext", "path-suffix", "path-contains", "path-glob"}
VALID_SAFETY = {"safe", "caution", "keep", "system-managed", "protected"}
VALID_ACTION = {"none", "delete", "command", "compact", "review"}
KNOWN_FIELDS = {
    "id", "name", "why", "recovery", "match", "category",
    "safety", "action", "command", "priority", "unless-below",
}


# ---------------------------------------------------------------- 匹配引擎

def glob_match(parts, text):
    """与 Rust 端等价：`*` 可跨分隔符，`?` 单字符，大小写不敏感。"""
    t = list(text)
    pi = ti = 0
    star_p = None
    star_t = 0
    while ti < len(t):
        if pi < len(parts):
            kind, val = parts[pi]
            if kind == "any_seq":
                star_p, star_t = pi, ti
                pi += 1
                continue
            if kind == "any_char":
                pi += 1
                ti += 1
                continue
            if val.lower() == t[ti].lower():
                pi += 1
                ti += 1
                continue
        if star_p is not None:
            pi = star_p + 1
            star_t += 1
            ti = star_t
        else:
            return False
    while pi < len(parts) and parts[pi][0] == "any_seq":
        pi += 1
    return pi == len(parts)


def compile_glob(pattern):
    out = []
    for c in pattern:
        if c == "*":
            if not out or out[-1][0] != "any_seq":
                out.append(("any_seq", None))
        elif c == "?":
            out.append(("any_char", None))
        else:
            out.append(("lit", c))
    return out


def basename(path):
    return path.rstrip("\\").rsplit("\\", 1)[-1]


def ext_of(name):
    if "." not in name:
        return None
    dot = name.rfind(".")
    if dot == 0 or dot == len(name) - 1:
        return None
    return name[dot + 1:]


def rel_lower(path):
    """把 C:\\Users\\x\\... 变成 users\\x\\...（小写），与 Snapshot::lower_dir_path 一致。"""
    p = path
    if len(p) >= 2 and p[1] == ":":
        p = p[2:]
    p = p.strip("\\")
    return p.replace("/", "\\").lower()


class Rule:
    def __init__(self, raw, order):
        self.raw = raw
        self.order = order
        self.id = raw["id"]
        self.name = raw.get("name", "")
        self.kind = raw["match"]["kind"]
        self.pattern = str(raw["match"]["pattern"])
        self.category = raw["category"]
        self.safety = raw["safety"]
        self.action = raw.get("action", "none")
        self.command = raw.get("command")
        self.priority = raw.get("priority", 0)
        # 语境约束：祖先路径含任一片段时，本规则不适用。
        # 用于挡「可重建」类规则在已安装软件目录里的误判。
        self.unless_below = [
            str(s).replace("/", "\\").lower() for s in (raw.get("unless-below") or [])
        ]

        p = self.pattern.lower()
        if self.kind == "name":
            self.matcher = ("name", p)
        elif self.kind == "ext":
            self.matcher = ("ext", p.lstrip("."))
        elif self.kind == "name-glob":
            self.matcher = ("glob_name", compile_glob(p))
        elif self.kind == "path-suffix":
            self.matcher = ("path_suffix", p.strip("\\"))
        elif self.kind == "path-contains":
            self.matcher = ("path_contains", p)
        elif self.kind == "path-glob":
            q = p.replace("/", "\\")
            if not q.startswith("*"):
                q = "*" + q
            self.matcher = ("glob_path", compile_glob(q))
        else:
            self.matcher = ("invalid", None)

    @property
    def is_path_rule(self):
        return self.matcher[0] in ("path_suffix", "path_contains", "glob_path")

    def applies_here(self, path_lower):
        """语境约束：只看祖先部分（去掉最后一段），避免片段与自身名字撞上。"""
        if not self.unless_below:
            return True
        if path_lower is None:
            return True
        i = path_lower.rfind("\\")
        ancestor = path_lower[:i] if i >= 0 else ""
        return not any(frag in ancestor for frag in self.unless_below)

    def match_dir_at(self, name, path_lower):
        """目录匹配：路径规则看整条相对路径，名字/通配规则看末段名字。"""
        if not self.applies_here(path_lower):
            return False
        kind, val = self.matcher
        if kind == "path_suffix":
            return path_lower == val or path_lower.endswith("\\" + val)
        if kind == "path_contains":
            return val in path_lower
        if kind == "glob_path":
            return glob_match(val, path_lower)
        if kind == "name":
            return name.lower() == val
        if kind == "glob_name":
            return glob_match(val, name)
        return False

    def match_file(self, path):
        kind, val = self.matcher
        if self.is_path_rule:
            return False
        if not self.applies_here(rel_lower(path)):
            return False
        base = basename(path)
        if kind == "name":
            return base.lower() == val
        if kind == "glob_name":
            return glob_match(val, base)
        if kind == "ext":
            e = ext_of(base)
            return e is not None and e.lower() == val
        return False


def attribute_path(rules, path, is_dir):
    """
    复刻 Rust 端 attribute_all 的语义：逐级下推，
    自己命中且 priority 不低于继承值时就覆盖，否则继承父目录。

    这条继承规则是整个分析层的核心 —— 直接决定
    「WinSxS 下面的东西算不算系统托管」这类判断。
    """
    rel = rel_lower(path)
    comps = [c for c in rel.split("\\") if c]

    file_comp = None
    if not is_dir:
        if not comps:
            return None
        file_comp = comps.pop()

    best = None
    cur = ""
    for c in comps:
        cur = c if not cur else cur + "\\" + c
        own = None
        for r in rules:
            if r.match_dir_at(c, cur):
                own = r
                break
        if own is not None and (best is None or own.priority >= best.priority):
            best = own

    if file_comp is not None:
        own = None
        for r in rules:
            if r.match_file(file_comp):
                own = r
                break
        if own is not None and (best is None or own.priority >= best.priority):
            best = own

    return best


def load_rules(path):
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    rules = [Rule(r, i) for i, r in enumerate(raw)]
    # 与 Rust 端一致：priority 降序的稳定排序
    rules.sort(key=lambda r: -r.priority)
    return rules


# ---------------------------------------------------------------- 校验

def check_structure(raw_list):
    errors, warns = [], []
    ids = set()
    for i, r in enumerate(raw_list):
        tag = r.get("id", f"#{i}")
        for f in REQUIRED:
            if f not in r:
                errors.append(f"[{tag}] 缺少必填字段 {f}")
        # 未知字段必须报错 —— serde 遇到不认识的字段会**静默**用默认值，
        # 于是规则悄悄失效（`unless-below` 写成 `unless_below` 就这么丢过）
        for k in r:
            if k not in KNOWN_FIELDS:
                errors.append(f"[{tag}] 未知字段 `{k}`（拼写错误？会被静默忽略）")
        if "match" in r:
            m = r["match"]
            if "kind" not in m or "pattern" not in m:
                errors.append(f"[{tag}] match 缺少 kind 或 pattern")
            else:
                for k in m:
                    if k not in ("kind", "pattern"):
                        errors.append(f"[{tag}] match 含未知字段 `{k}`")
                if m["kind"] not in VALID_KINDS:
                    errors.append(f"[{tag}] 未知的 kind: {m['kind']}")
                if not str(m["pattern"]).strip():
                    errors.append(f"[{tag}] pattern 为空")
        if r.get("safety") and r["safety"] not in VALID_SAFETY:
            errors.append(f"[{tag}] 未知的 safety: {r['safety']}")
        if r.get("action") and r["action"] not in VALID_ACTION:
            errors.append(f"[{tag}] 未知的 action: {r['action']}")
        if r.get("action") == "command" and not r.get("command"):
            errors.append(f"[{tag}] action=command 但没有填 command")
        if not r.get("why"):
            warns.append(f"[{tag}] 没有写 why，报告里会缺一句解释")
        if not r.get("recovery"):
            warns.append(f"[{tag}] 没有写 recovery")
        if r["id"] in ids:
            errors.append(f"[{tag}] id 重复")
        ids.add(r["id"])
    return errors, warns


def check_context_sets(raw_list):
    """
    校验 `unless-below` 的重复列表与锚点定义保持一致。

    有两条规则（delivery-optimization-dir / dotnet-vs-temp）位于锚点定义之前，
    只能手工写全列表。这种"同一份数据写两遍"的地方最容易失同步，
    所以必须校验。
    """
    problems = []
    base = None
    base_id = None
    for r in raw_list:
        ub = r.get("unless-below")
        if not ub:
            continue
        norm = sorted(str(x).replace("/", "\\").lower() for x in ub)
        if base is None:
            base, base_id = norm, r["id"]
        elif norm != base:
            problems.append(
                f"规则 `{r['id']}` 的 unless-below 与 `{base_id}` 不一致\n"
                f"    仅在前者中出现: {sorted(set(norm) - set(base))}\n"
                f"    仅在后者中出现: {sorted(set(base) - set(norm))}"
            )
    return problems


def check_shadowing(rules):
    """找出永远不可能生效的规则。"""
    problems = []
    for j, b in enumerate(rules):
        for a in rules[:j]:
            if a.is_path_rule and not b.is_path_rule:
                continue
            if not a.is_path_rule and b.is_path_rule:
                continue

            # path-suffix: A 的模式是 B 模式的后缀 → A 通吃 B
            if a.matcher[0] == "path_suffix" and b.matcher[0] == "path_suffix":
                pa, pb = a.matcher[1], b.matcher[1]
                if pa != pb and pb.endswith("\\" + pa):
                    problems.append(
                        f"规则 `{b.id}`（{b.pattern}）被更早的 `{a.id}`（{a.pattern}）完全遮蔽"
                    )
                    break

            # name / name-glob: A 若能匹配 B 的精确名字，则 B 永远轮不到
            if a.matcher[0] in ("glob_name",) and b.matcher[0] == "name":
                if glob_match(a.matcher[1], b.matcher[1]):
                    problems.append(
                        f"规则 `{b.id}`（名字 {b.pattern}）被更早的 `{a.id}`（{a.pattern}）遮蔽"
                    )
                    break
    return problems


# ---------------------------------------------------------------- 语义测试样本

# (路径, 是否目录, 期望规则 id)
CASES = [
    # 系统硬保护
    (r"C:\Windows\WinSxS\Manifests", True, "windows-winsxs"),
    (r"C:\Windows\System32\config", True, "windows-system32"),
    (r"C:\Windows\System32\DriverStore\FileRepository", True, "windows-driverstore"),
    (r"C:\Windows\SysWOW64", True, "windows-syswow64"),
    (r"C:\Program Files\SomeApp", True, "program-files"),
    (r"C:\Program Files (x86)\OldApp", True, "program-files-x86"),
    (r"C:\Windows\Installer", True, "windows-installer"),

    # 系统托管
    (r"C:\hiberfil.sys", False, "hiberfil"),
    (r"C:\pagefile.sys", False, "pagefile"),
    (r"C:\swapfile.sys", False, "swapfile"),
    (r"C:\System Volume Information", True, "system-volume-information"),

    # 系统更新残留
    (r"C:\Windows.old\Windows", True, "windows-old"),
    (r"C:\Windows\SoftwareDistribution\Download", True, "softwaredistribution-download"),
    (r"C:\$Recycle.Bin\S-1-5-21", True, "recycle-bin"),
    (r"C:\Windows\Temp", True, "windows-temp"),

    # 开发工具缓存
    (r"C:\Users\me\AppData\Local\npm-cache\_cacache", True, "npm-cache"),
    (r"C:\Users\me\AppData\Local\pip\Cache\wheels", True, "pip-cache"),
    (r"C:\Users\me\AppData\Local\Yarn\Cache\v6", True, "yarn-cache"),
    (r"C:\Users\me\.cargo\registry\cache\index", True, "cargo-registry-cache"),
    (r"C:\Users\me\.cargo\registry\src\github.com", True, "cargo-registry-src"),
    (r"C:\Users\me\.gradle\caches\modules-2", True, "gradle-caches"),
    (r"C:\Users\me\.m2\repository\org", True, "maven-repository"),
    (r"C:\Users\me\go\pkg\mod\cache\download", True, "go-mod-cache"),
    (r"C:\Users\me\AppData\Local\uv\cache", True, "uv-cache"),
    (r"C:\Users\me\AppData\Local\Temp", True, "user-temp"),

    # IDE
    (r"C:\Users\me\AppData\Local\JetBrains\IntelliJIdea2024.1\caches", True, "jetbrains-caches"),
    (r"C:\Users\me\AppData\Roaming\Code\Cache", True, "vscode-cache"),
    (r"C:\Users\me\AppData\Roaming\Code\CachedData\abc", True, "vscode-cacheddata"),
    (r"C:\Users\me\AppData\Roaming\Code\logs\20240101", True, "vscode-logs"),
    (r"C:\Users\me\AppData\Roaming\Cursor\Cache", True, "cursor-cache"),
    (r"C:\proj\.vs\proj", True, "dotnet-vs-temp"),

    # 浏览器
    (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Cache", True, "chrome-cache"),
    (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\Code Cache", True, "chrome-code-cache"),
    (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\Default\GPUCache", True, "chrome-gpu-cache"),
    (r"C:\Users\me\AppData\Local\Microsoft\Edge\User Data\Default\Cache", True, "edge-cache"),
    (r"C:\Users\me\AppData\Local\Microsoft\Edge\User Data\Default\GPUCache", True, "edge-gpu-cache"),
    (r"C:\Users\me\AppData\Local\Google\Chrome\User Data\GrShaderCache", True, "shader-cache"),
    (r"C:\Users\me\AppData\Local\Microsoft\Edge\User Data\ShaderCache", True, "shader-cache"),
    (r"C:\Users\me\AppData\Local\Mozilla\Firefox\Profiles\abc.default\cache2", True, "firefox-cache"),

    # 构建产物
    (r"D:\proj\web\node_modules\react", True, "node-modules"),
    (r"D:\proj\web\__pycache__", True, "pycache"),
    (r"D:\proj\app\.next\cache", True, "nextjs-cache"),
    (r"D:\proj\rust\target\debug", True, "rust-target"),
    (r"D:\proj\py\.venv\Lib", True, "python-venv"),
    (r"D:\proj\web\dist\assets", True, "dist"),

    # 用户数据
    (r"C:\Users\me\Documents", True, "user-documents"),
    (r"C:\Users\me\Downloads", True, "user-downloads"),
    (r"C:\Users\me\Pictures", True, "user-pictures"),
    (r"C:\Users\me\OneDrive\Docs", True, "onedrive"),
    (r"D:\SteamLibrary\steamapps\common\Game", True, "steam-games"),

    # 本土应用
    (r"C:\Users\me\Documents\WeChat Files\wxid_abc\FileStorage", True, "wechat-files"),
    (r"C:\Users\me\Documents\xwechat_files\wxid_abc", True, "wechat-xwechat"),
    (r"C:\Users\me\Documents\Tencent Files\12345", True, "qq-files"),
    (r"C:\Users\me\AppData\Roaming\Tencent\WeChat\All Users\Cache", True, "wechat-cache"),
    (r"C:\Users\me\AppData\Local\BaiduNetdisk\cache", True, "baidu-netdisk"),

    # 虚拟磁盘（文件）
    (r"C:\Users\me\AppData\Local\Packages\CanonicalGroupLimited\LocalState\ext4.vhdx", False, "wsl-vhdx"),
    (r"D:\VMs\win11.vhdx", False, "vhdx-generic"),

    # 转储 / 日志
    (r"C:\Windows\Minidump", True, "windows-minidump"),
    (r"C:\Windows\MEMORY.DMP", False, "memory-dmp"),
    (r"C:\Windows\LiveKernelReports", True, "windows-livekernelreports"),
    (r"D:\logs\app.log", False, "log-files"),
    (r"C:\Users\me\AppData\Local\Microsoft\Windows\Explorer\thumbcache_1024.db", False, "thumbcache"),

    # 兜底
    (r"C:\Users\me\AppData\Roaming", True, "appdata-roaming"),
    (r"C:\Users\me\AppData\Local", True, "appdata-local"),
    (r"C:\ProgramData\SomeApp", True, "programdata"),

    # 明确不该被"清理规则"命中的
    (r"C:\Windows\Fonts", True, "windows-fonts"),

    # 根据一次真实扫描的「未识别清单」补出来的规则
    (r"C:\Users\me\NTUSER.DAT", False, "ntuser-dat"),
    (r"C:\Users\me\ntuser.dat.LOG1", False, "ntuser-dat"),
    (r"C:\Users\me\UsrClass.dat", False, "usrclass-dat"),
    (r"C:\Users\me\.workbuddy\memory\x.md", False, "workbuddy-data"),
    (r"D:\proj\.git\objects", True, "git-repo"),
    (r"C:\Users\me\.rustup\toolchains", True, "rustup-toolchains"),
    (r"C:\Users\me\.cargo\bin", True, "cargo-home"),
    (r"C:\Users\me\.cargo\registry\cache", True, "cargo-registry-cache"),
    (r"C:\Users\me\.stm32cubemx\Repository", True, "stm32cubemx"),
    (r"C:\Users\me\STM32Cube\Repository", True, "stm32cube"),
    (r"C:\Users\me\.espressif\frameworks", True, "espressif"),
    (r"C:\Users\me\.venv-html-to-docx\Lib", True, "venv-glob"),
    (r"C:\Users\me\.qoder\cache", True, "ai-ide-qoder"),
    (r"C:\Users\me\.trae-cn\logs", True, "ai-ide-trae"),
    (r"C:\Users\me\.codeium\index", True, "ai-extension-cache"),
    (r"C:\Users\me\CrossDevice\Phone", True, "cross-device"),
    (r"C:\Users\me\WPS Cloud Files\doc", True, "wps-cloud-files"),
    (r"C:\Users\me\AppData\Local\Packages\SomeApp", True, "appdata-packages"),
    (r"C:\Users\me\AppData\Local\Microsoft\Windows\INetCache", True, "inetcache"),
    (r"C:\Users\me\AppData\Local\CrashDumps", True, "appdata-crashdumps"),
    (r"D:\pic\Thumbs.db", False, "thumbs-db"),
    (r"D:\pic\.DS_Store", False, "ds-store"),
    (r"D:\unzip\__MACOSX\a", True, "macosx-resource"),

    # ---- 数据盘（D:）场景，来自一次真实扫描 ----
    # 关键回归：装在自定义安装目录里的 node_modules 绝不能被判成
    # 「可安全回收」—— 那是软件自带的运行时依赖，删了软件就废了。
    (r"D:\Program\MyApp\resources\app.asar.unpacked\node_modules", True, "custom-program-dir"),
    (r"D:\Program\Microsoft VS Code\resources\app\extensions\copilot\node_modules", True, "ide-extensions"),
    (r"D:\Program\Trae CN\resources\app\node_modules", True, "custom-program-dir"),
    (r"D:\Program\SomeApp\app.exe", False, "custom-program-dir"),

    (r"D:\Program Files\App\node_modules", True, "program-files"),
    (r"D:\Matlab2022b\bin\win64", True, "matlab-install"),
    (r"D:\keil_v5\ARM\PACK", True, "keil-install"),
    (r"D:\WindowsApps\SomeApp_1.0_x64", True, "windows-apps-drive"),
    (r"D:\SolidWorks_Flexnet_Server\lmgrd.exe", False, "solidworks-license"),

    (r"D:\WeGameApps\LOL\Game", True, "wegame-apps"),
    (r"D:\迅雷下载\ubuntu.iso", False, "xunlei-download"),
    (r"D:\下载\setup.exe", False, "cn-downloads"),
    (r"D:\BaiduNetdiskDownload\movie.mp4", False, "baidu-netdisk-download"),
    (r"D:\桌面\论文.docx", False, "desktop-relocated"),
    (r"D:\备份2\2024", True, "backup-dirs"),
    (r"D:\备份\old", True, "backup-dirs"),
    (r"D:\backup2024\data", True, "backup-dirs-en"),
    (r"D:\蓝桥杯嵌入式资料\真题", True, "study-materials"),
    (r"D:\51单片机入门教程资料\视频", True, "study-materials"),
    (r"D:\学习\俄罗斯方块", True, "study-dir"),
    (r"D:\travel photo\2023", True, "travel-photos"),
    (r"D:\ai发票\2024.pdf", False, "invoice-files"),
    (r"D:\CloudMusic\song.mp3", False, "cloud-music-cache"),
    (r"D:\whisper_models\large.bin", False, "ai-model-weights"),
    (r"D:\Config.Msi\abc.rbf", False, "config-msi"),
    (r"D:\DeliveryOptimization\cache", True, "delivery-optimization-dir"),
    (r"D:\Espressif\frameworks", True, "espressif-drive"),
    (r"D:\jianying\Project", True, "video-project-dir"),

    # 家庭照片必须被保护住 —— 这是最不能出错的一类
    (r"D:\老妈照片\合影.jpg", False, "family-photos"),
    (r"D:\照片\2020", True, "family-photos"),
    (r"D:\我的图片\a.png", False, "pics-dir"),
    (r"D:\视频素材\a.mp4", False, "video-dir-cn"),
    (r"D:\Ros教程\第1讲.mp4", False, "course-videos"),
    (r"D:\VMware17.5\x64", True, "vmware-install"),
    (r"D:\LibreOffice\program", True, "libreoffice-install"),
    (r"D:\Pandoc\pandoc.exe", False, "pandoc-install"),
    (r"D:\LenovoSoftstore\Store", True, "lenovo-store"),
    (r"D:\LeStoreDownload\app.exe", False, "lenovo-download"),
    (r"D:\Lanqiao\真题", True, "embedded-lanqiao"),
    (r"D:\cc2530\例程", True, "embedded-chip-dirs"),
    (r"D:\ESP8266固件与烧写\bin", True, "esp-firmware"),
    (r"D:\Codex\runtimes", True, "ai-tool-dir-generic"),
    (r"D:\TREA\npm-global", True, "ai-tool-trae-generic"),

    # ---- 关键回归：IDE 扩展目录里的依赖绝不能判「可安全回收」 ----
    # 这是真实事故：这些路径曾被 node-modules 规则判成 safe，
    # 而它们是扩展自带的运行时依赖，删了扩展直接失效。
    (r"C:\Users\me\.qoder\extensions\ms-python.python-2025.16.0-win32-x64\out\client\node_modules", True, "ide-extensions"),
    (r"C:\Users\me\.vscode\extensions\cl.keil-assistant-1.7.0\node_modules", True, "ide-extensions"),
    (r"C:\Users\me\.vscode\extensions\ms-python.python-2026.4.0-win32-x64\out\client\node_modules", True, "ide-extensions"),
    (r"C:\Users\me\.cursor\extensions\some.ext-1.0\node_modules", True, "ide-extensions"),

    # 但**用户自己的项目**里的 node_modules 必须仍然可回收
    (r"D:\myproject\node_modules\react", True, "node-modules"),
    (r"D:\myproject\packages\app\node_modules\lodash", True, "node-modules"),
    (r"D:\myproject\node_modules\a\node_modules\b", True, "node-modules"),

    # 应用捆绑运行时里的 node_modules 同样不能删
    # 注：`ide-extensions`(96) 比 `custom-program-dir`(95) 更具体，所以
    # `...\resources\app\extensions\...` 归到前者 —— 两者都是"不清理"，
    # 取更具体的判定语义更清楚。
    (r"C:\Users\me\AppData\Local\Programs\SomeApp\resources\app\node_modules", True, "user-programs-dir"),
    (r"C:\Users\me\AppData\Local\SomeApp\runtimes\cua_node\bin\node_modules", True, "app-runtimes"),
    (r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\resources\app\node_modules", True, "user-programs-dir"),

    # 用户项目里的构建产物仍然可回收
    (r"D:\myproject\.pytest_cache", True, "pytest-cache"),
    (r"D:\myproject\coverage", True, "coverage"),
    (r"D:\myproject\.next\cache", True, "nextjs-cache"),
    (r"D:\myproject\src\__pycache__", True, "pycache"),
]


def run_cases(rules):
    passed, failed = [], []
    for path, is_dir, expected in CASES:
        hit = attribute_path(rules, path, is_dir)
        got = hit.id if hit else None
        if got == expected:
            passed.append((path, expected))
        else:
            failed.append((path, is_dir, expected, got))
    return passed, failed


def coverage(rules):
    by_safety = defaultdict(int)
    by_category = defaultdict(int)
    for r in rules:
        by_safety[r.safety] += 1
        by_category[r.category] += 1
    return by_safety, by_category


def main():
    raw_list = yaml.safe_load(RULES_FILE.read_text(encoding="utf-8"))
    rules = load_rules(RULES_FILE)

    print(f"规则库: {RULES_FILE}")
    print(f"规则条数: {len(rules)}")
    print()

    errors, warns = check_structure(raw_list)
    print("── 结构校验 ──")
    if errors:
        for e in errors:
            print(f"  ✗ {e}")
    else:
        print("  ✓ 全部通过")
    if warns:
        print(f"  （{len(warns)} 条提醒，非致命）")
    print()

    problems = check_shadowing(rules)
    print("── 顺序遮蔽检测 ──")
    if problems:
        for p in problems:
            print(f"  ✗ {p}")
    else:
        print("  ✓ 没有规则被更宽泛的规则完全遮蔽")
    print()

    ctx_problems = check_context_sets(raw_list)
    print("── 语境约束一致性 ──")
    if ctx_problems:
        for p in ctx_problems:
            print(f"  ✗ {p}")
    else:
        n_ctx = sum(1 for r in raw_list if r.get("unless-below"))
        print(f"  ✓ {n_ctx} 条规则的 unless-below 列表一致")
    print()

    passed, failed = run_cases(rules)
    print("── 匹配语义验证 ──")
    print(f"  通过 {len(passed)} / {len(passed) + len(failed)}")
    if failed:
        print()
        for path, is_dir, expected, got in failed:
            kind = "目录" if is_dir else "文件"
            print(f"  ✗ [{kind}] {path}")
            print(f"      期望 {expected}  实际 {got}")
    print()

    by_safety, by_category = coverage(rules)
    print("── 安全等级分布 ──")
    for k in ["safe", "caution", "keep", "system-managed", "protected"]:
        if by_safety.get(k):
            print(f"  {k:<16} {by_safety[k]}")
    print()
    print("── 类别分布 ──")
    for k, v in sorted(by_category.items(), key=lambda x: -x[1]):
        print(f"  {k:<24} {v}")
    print()

    if errors or problems or ctx_problems or failed:
        print(
            f"结果: 失败（{len(errors)} 结构错误 / {len(problems)} 遮蔽 / "
            f"{len(ctx_problems)} 语境不一致 / {len(failed)} 用例不符）"
        )
        return 1
    print("结果: 全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
