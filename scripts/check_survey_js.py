"""校验 survey 生成的 HTML 里内嵌的 JS 语法。

之所以需要这一步：**HTML 里的 JS 编译期查不出来**。
Rust 侧只管把字符串拼好，JS 写错了照样编译通过，
只有浏览器打开才发现页面白屏 —— 而那时用户已经在用了。

做法：抽出 <script> 内容、把内嵌的大 JSON 换成小占位，
交给 node --check 做真正的语法解析。
"""
import json
import pathlib
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _paths  # noqa: E402

# 路径一律由 _paths 解析 —— 这里原本硬编码了绝对路径，项目一搬就全失效
HTML = _paths.survey_html()
TMP = _paths.work_dir("jscheck") / "_jscheck.js"
NODE = str(_paths.require_node())

if not HTML.exists():
    print(f"找不到 {HTML}")
    print("  界面文件由 survey 命令生成；也可以 set DD_SURVEY_HTML=<路径> 指定")
    sys.exit(1)

html = HTML.read_text(encoding="utf-8")

scripts = re.findall(r"<script>(.*?)</script>", html, re.S)
if not scripts:
    print("✗ HTML 里没有 <script> 块")
    sys.exit(1)

js = max(scripts, key=len)
print(f"HTML {len(html)/1024:.0f} KB ｜ JS {len(js):,} 字符 ｜ {len(scripts)} 个 script 块")

# 把内嵌数据替换成等结构的占位，避免 node 解析几百 KB 字面量
placeholder = (
    "const DATA={items:[],summary:[],root:'',file_count:0,dir_count:0,"
    "scanned_at:'',unattributed:0,folded_count:0,folded_bytes:0};"
)
js2, n = re.subn(r"const DATA = .*?;\n", placeholder + "\n", js, count=1, flags=re.S)
if n == 0:
    print("⚠ 没找到 `const DATA = ...;` —— 模板结构可能变了，请检查")

TMP.write_text(js2, encoding="utf-8")

r = subprocess.run([NODE, "--check", str(TMP)], capture_output=True, text=True)
if r.returncode == 0:
    print("✓ JS 语法正确")
    # 再做几项结构性检查：这些是"语法对但功能坏"的隐患
    checks = [
        ("细分渲染函数", "function renderKids("),
        ("细分折叠容器", 'className = "kids"'),
        ("子项可勾选渲染", "k.selectable"),
        ("判定差异标注", "differs_from_parent"),
        ("路径索引（含子项）", "const ALL_BY_PATH"),
        ("勾选集合", "const picked = new Set()"),
        ("导出处理器", 'getElementById("export")'),
        ("复制命令处理器", 'closest("button.copy")'),
        ("目录真实大小说明", "dir_total"),
        # ---- 视觉层：构成条与占比 ----
        ("构成条渲染", 'getElementById("comp")'),
        ("构成条分段与图例", 'class="bar"'),
        ("卡片占比条", 'class="share"'),
        ("语义色映射表", "const SAFETY_VAR"),
        ("占比合计保护（防除零）", "Math.max(1,"),
        ("子项条形图按等级着色", 'class="${k.safety}"'),
        # ---- 路径可点击（打开资源管理器）----
        ("服务信息常量", "const SERVER ="),
        ("浮层容器", "function ensurePop"),
        ("浮层渲染", "function showPathPop"),
        ("路径点击委托", 'closest(".pth")'),
        ("跳转信标", "function beacon"),
        # ---- 服务健康检查（服务会主动消失，不能留死按钮）----
        ("健康检查函数", "function pingService"),
        ("可达状态变量", "let SERVICE_UP"),
        ("探测超时兜底", "setTimeout(() => done(false)"),
        ("依据可达状态决定行为", 'SERVICE_UP === true'),
        ("浮层定位", "function positionPop"),
        # 静态模式必须给出替代方案而不是留个死按钮
        # （JS 里是反引号模板：`explorer "${path}"`，引号前没有反斜杠）
        ("静态模式退路", 'explorer "${path}"'),
    ]
    print()
    print("结构检查：")
    bad = 0
    for name, needle in checks:
        ok = needle in js
        print(f"  {'✓' if ok else '✗'} {name}")
        if not ok:
            bad += 1
    # 括号配平（粗查，能抓到大部分漏括号）
    for open_c, close_c, label in [("{", "}", "花括号"), ("(", ")", "圆括号"), ("[", "]", "方括号")]:
        d = js.count(open_c) - js.count(close_c)
        if d != 0:
            print(f"  ✗ {label}不配平：差 {d}")
            bad += 1
    if bad == 0:
        print("  ✓ 全部通过")
    sys.exit(0 if bad == 0 else 1)
else:
    print("✗ JS 语法错误：")
    print(r.stderr[:2000])
    sys.exit(1)
