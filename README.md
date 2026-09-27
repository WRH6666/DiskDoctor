# DiskDoctor

[![CI](https://github.com/WRH6666/DiskDoctor/actions/workflows/ci.yml/badge.svg)](https://github.com/WRH6666/DiskDoctor/actions/workflows/ci.yml)

> 硬盘空间体检工具。不是又一个 treemap 查看器 —— 它回答的是「**这里能不能删**」。

只支持 **Windows**。完整方案见 [`docs/方案设计-磁盘空间治理工具.md`](docs/方案设计-磁盘空间治理工具.md)。

---

## 直接用（不需要装 Rust）

到 [Releases](../../releases/latest) 下载 **`DiskDoctor-v0.1.1-win64.zip`**，
解压后**双击 `DiskDoctor.cmd`**，会出现菜单：

```
  [1] 盘点磁盘内容        <- 第一次用先点这个
      扫描完自动打开浏览器界面，里面能看清每个目录是什么、
      能不能删，点任意路径可以直接跳到那个文件夹。
  [2] 执行清理            在界面里勾选导出后，回来点这个
  [3] 查看 / 还原暂存区    清理掉的东西可以整体撤回来
  [9] 换个扫描位置
  [0] 退出
```

不需要命令行，不需要安装任何东西（只用系统自带的 PowerShell）。

> ### 为什么要双击 `.cmd`，而不是直接双击 `.exe`
>
> `diskdoctor.exe` 是**命令行程序**。双击它等于"不带参数运行" ——
> 它会扫描当前目录、把结果打印到控制台，然后**进程结束、窗口关闭**，
> 你只会看到界面一闪而过、什么都没留下。
>
> `.cmd` 会停住窗口并给你菜单。这是本工具唯一的图形入口。
>
> 想自己敲命令的，也可以只下载 `diskdoctor.exe`：

```bat
:: 生成可浏览的决策界面（浏览器会打开，界面上点路径可直接跳到该文件夹）
diskdoctor.exe survey "C:\Users\你的用户名" --open

:: 只读体检，输出 Markdown 报告
diskdoctor.exe "C:\" --out report.md

:: 看全部用法
diskdoctor.exe --help
```

> 建议以**管理员身份**运行：否则 `$Recycle.Bin`、部分 `AppData`、卷影副本
> 等位置读不到，报告会标注「因权限跳过 N 处」。

扫描结果会写到程序旁边的 `DiskDoctor-输出\`（程序目录不可写时才退到
`%LOCALAPPDATA%\DiskDoctor\out`）—— 所以你能立刻找到它。

---

## 现在能做什么

**第一步 · 看清装了什么** —— 生成一个可浏览、可挑选的界面：

```bash
diskdoctor survey "C:\Users\你"
```

生成一个自包含的 HTML（双击就能在浏览器打开，不用装东西、不联网）。
在里面可以：

- 按体积 / 类别 / 安全等级**筛选和排序**
- 每条都看到：完整路径、体积、**能否删的判定**、判定理由、删了怎么恢复
- **超过 5 GB 的条目可以展开**，看清里面装了什么（见下）
- **自己勾选**要清理的（只有真正能清理的才给勾选框）
- 勾完**导出清单**，再用下面的命令执行

界面**自己不删任何东西** —— 它只让你挑。这样"看清 → 判断 → 决定 → 执行"
里最需要人参与的前三步，都在你手里。

### 大条目可以展开看内部构成

`AppData\Local` 这类目录在界面上原本只是"17.63 GB / 需确认"一行 ——
你知道它大，却不知道里面是什么，也就无从判断。

所以超过阈值（默认 5 GB）的条目会**逐层细分**：

```
17.63 GB  C:\Users\你\AppData\Local          需确认  [展开看内部构成（40 项）]
  ├ 7.64 GB  35.2%  JianyingPro        需确认          ← 剪映缓存
  ├ 4.09 GB  18.9%  NVIDIA             需确认
  ├ 3.68 GB  17.0%  Microsoft          需确认
  ├ 571.9 MB  2.6%  Temp               可安全回收  ☑   ← ▲ 判定与上层不同
  └ …
```

几个设计要点：

- **`▲` 标记判定与上层不同的项。** 这是最有价值的信号 ——
  父目录整体"需确认"，但里面藏着可安全回收的块，标出来你才不会漏掉。
- **子项也能勾选。** 看到里面 571 MB 是可回收的，直接勾它。
  执行时同样会重新过护栏校验，所以勾子项不比勾父项危险。
- **占比之和恒为 100%。** 这里有个坑：条目的 `size` 是**归因区域**大小
  （只算归本规则管的部分），而子项来自目录树、是**真实大小**。
  混用会让占比超过 100%（实测见过 154%），用户只会认为工具算错了。
  所以细分时改用目录真实总大小做分母，并在标题里说明差额去哪了：

  > 该目录实际共 1.20 GB，其中 430.0 MB 归属其他规则（已单独列出）

用 `--expand-over-gb N` 调阈值，`--expand-limit N` 限制每层展开多少项。

### 界面长什么样

从上到下七层，顺序是按"先看什么"排的：

```
【1】标题 + 扫描范围
【2】四张汇总卡片        每张一个大数字 + 一条"占全盘多少"
【3】构成条              一条横向堆叠条 + 图例 ← 一眼看出能清多少
【4】提示                折叠了什么、哪些没识别
【5】工具栏              搜索 ｜ 等级筛选片 ｜ 类别 ｜ 排序
【6】列表                每行：路径（可点）·判定·理由·体积
【7】底栏                已选统计 + 三个动作（固定底部，随时可达）
```

**构成条是这轮新加的**，因为四张卡片各说各的体积时，「39 GB 禁止触碰」
和「1.4 GB 可安全回收」看不出差别。画成一条堆叠条，结论就变得无法忽略：

```
▏1.5%▏████████████████████████████████████ 83.8% ████ 8.3% ███ 6.3%
可安全回收 1.13 GB   需确认 61.97 GB   用户数据 6.13 GB   禁止触碰 4.69 GB
```

**视觉上的几个决定**

| 决定 | 理由 |
|---|---|
| 底色分三层（页面 / 面板 / 抬升） | 深色界面靠**色阶**区分层级比靠边框更干净 |
| 描边用半透明白而非实色灰 | 实色描边会和面板色互相干扰，换底色还得跟着调 |
| 数字一律等宽（`tabular-nums`） | 这一页到处是可比数值，比例字体下「1」比「8」窄，竖排会参差不齐 |
| 卡片顶部一道语义色细带 | 不用读文字就知道这张卡属于哪一档 |
| **子项占比条的颜色跟着安全等级走** | 这样"哪块能清、哪块不能"不用读文字就能看出来 |
| 选中态用左侧竖条而非整块变色 | 更克制，但扫一眼就能看到选了哪些 |
| 锁定项压暗到 58% 而非隐藏 | 用户仍要看得到"这里有东西，只是不归我管" |

### 界面上的三种呈现

| 呈现 | 含义 | 你能做什么 |
|---|---|---|
| ☑ 勾选框 | 处置方式是"移入暂存区" | 可以勾选 |
| 可复制的命令 | 该用官方命令处理（如 `pip cache purge`） | 点一下复制，自己去跑 |
| 只有说明 | 系统托管 / 用户数据 / 禁止触碰 | 告诉你该怎么处理，不给操作 |

第二种是刻意的：像 pip / npm 缓存，官方命令除了清文件还会维护索引状态。
**既然规则说了该用命令，界面就不提供另一种做法** —— 避免用错方式留下不一致的状态。

### 点路径 → 直接跳过去看

界面上**每一条路径都可以点，点一下就打开**：

| 点的是什么 | 会发生什么 |
|---|---|
| 目录 | 打开该目录的资源管理器窗口 |
| 文件 | 打开它**所在的文件夹**并选中它 —— 而不是用默认程序打开文件本身（`.docx` / `.exe` 一打开就启动别的程序，太重） |
| **右键** 或 **Ctrl + 点击** | 只复制路径，不打开窗口 |

点完右下角会浮出一条提示，写明打开了哪条路径 —— 免得连点几下之后
分不清哪个窗口是哪个。

这个功能解决的是"我知道这块占 7.5 GB，但我不知道里面是什么"——
清单告诉你**体积**，跳过去看一眼才知道**该不该留**。

实现上有个绕不开的限制：**浏览器不允许静态页面打开本地文件夹**
（导航到 `file://` 目录会被拦，也拿不到资源管理器）。所以需要本地服务代劳：

```bash
diskdoctor survey "C:\Users\你的用户名" --serve --open
```

```bash
--serve   启用路径跳转，命令会常驻（Ctrl+C 或 10 分钟无操作后停止）
          不启用时页面退化为静态，点路径只能复制 —— 不会留一个点了没反应的按钮
```

桌面启动器的 `[1]` 会自动带上 `--serve`，并且**在后台起服务**，
所以菜单不会卡住，你可以随时回来点 `[2]`。

#### 这个服务的安全边界

它能让网页调用本机进程，所以按不可信输入来防，一共三层：

| 层 | 挡什么 |
|---|---|
| **随机令牌**（32 字节） | 别的网页读不到你的本地 HTML，拿不到令牌 |
| **路径必须在扫描根内** | 先规范化再按路径分量比前缀 —— `..` 逃逸和 `C:\root2` 冒充 `C:\root` 都过不去 |
| **校验 `Host` 头** | 防 DNS rebinding（把域名解析到 127.0.0.1 来绕过同源限制） |

再加上：只绑 `127.0.0.1`（局域网连不上）、**只做「打开 / 定位」两种只读动作**
（它没有任何删除或修改文件的能力）、请求头有大小上限、并发有上限。

这些边界都有测试：`scripts/check_serve.py`（20 项）覆盖安全边界 10 项 +
健康检查 4 项 + 正常路径 5 项 + 空闲退出 1 项；
`scripts/check_path_open.py` 另外验证"点一下真的会弹出窗口"。

#### 服务会自己停止 —— 界面知道这件事

空闲 10 分钟（或你关掉那个进程）服务就没了。这时点路径本该"没反应"，
而用户不知道原因。所以界面启动时会做一次健康检查：

```
磁盘内容.html 打开
  └─ 探一次 a=ping（服务返回一张真实图片）
       ├─ 通了   → 单击路径 = 直接打开文件夹
       └─ 不通   → 单击路径 = 弹浮层，给可粘贴的 explorer "..." 命令
                   并说明是「没启动服务」还是「服务已停止」
```

**探针必须返回真图片**：非图片响应（204、403、代理返回的 502）都会让
`<img>` 触发 `onerror`，分不出"服务在"和"服务不在"。

#### 关于系统代理

如果你的机器上开着代理工具，**浏览器不受影响** —— 它读的是系统代理的
绕过列表，默认就含 `localhost` 和 `127.*`。

但**命令行程序（python / curl）会受影响**：它们读 `http_proxy` 环境变量，
**连 127.0.0.1 的请求也会发给代理**。所以用脚本测这个服务时必须显式直连：

```python
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
```

`scripts/check_serve.py` 就是这么做的 —— 这是它里面最容易踩的坑。

### 然后执行你的选择

```bash
# 先预览（不动文件）
diskdoctor clean --from-selection 导出的清单.json --dry-run

# 确认后执行 —— 移入暂存区，可撤销
diskdoctor clean --from-selection 导出的清单.json --yes
```

**你的勾选只是"意图"，不是授权。** 执行时每一项都会**重新过一遍护栏和规则判定**：

- 手工往清单里塞 `C:\Windows` → 拒绝
- 把某条的 `safety` 改成 `safe` 想蒙混 → 无效，仍按规则重新判定
- 判定在生成清单后变严了（规则库更新）→ 拒绝并说明变化
- 重复项自动去重；被已选父目录覆盖的子项会自动跳过（移动父目录时已一并带走）

所以即使清单被篡改，也删不掉不该删的东西。

---

## 装到桌面（可选）

上面的 zip 解压后双击 `DiskDoctor.cmd` 就能用了。这一节是把它**装成桌面图标
+ 开始菜单项** —— 适合想长期用、不想每次都去找那个文件夹的情况。

```bash
python assets/install.py              # 安装：拷文件 + 建快捷方式（会回读验证）
python assets/install.py --uninstall  # 卸载：全部送进回收站，不是硬删
```

装到 `%LOCALAPPDATA%\Programs\DiskDoctor`（用户级程序的标准位置，**不需要管理员**），
在**桌面**和**开始菜单**各放一个快捷方式。双击打开的是同一个小菜单：

```
  [1] 盘点磁盘内容      生成界面，看清每个目录是什么、能不能删
  [2] 执行清理          用你在界面里勾选导出的清单跑（先试运行，再确认）
  [3] 查看 / 还原暂存区  清理过的东西都在这里，可以整体撤回来
  [9] 换个扫描位置
  [0] 退出
```

> 这一步需要 **Python**（用来跑 `install.py`）。只是想用这个工具的话不需要 ——
> 下载上面的 zip 解压即可。安装完成后 Python 也不再需要，启动器本身是
> PowerShell 写的。

配一个自绘的图标（`assets/make_icon.py` 生成，一个仪表盘环 + 指针，
16×16 下仍可辨认）。想要**便携版**的图标最简办法：右键 `DiskDoctor.cmd`
→ 发送到 → 桌面快捷方式。

### 几条实现上的讲究

**系统特殊目录要问系统，不能拼路径。** 这是本项目踩过的坑：最初用
`%USERPROFILE%\Desktop` 拼桌面路径，而本机桌面实际被重定向到了 `D:\桌面`
（注册表 `User Shell Folders\Desktop`）—— 那个 `C:\Users\...\Desktop`
只是个**过时的空壳目录**，东西放进去桌面上根本不显示。

**同一个坑还影响过清单查找**：启动器原本拼 `%USERPROFILE%\Downloads` 找
浏览器导出的清单，而本机下载在 `D:\下载` —— 结果「导出清单 → 执行」
这条路直接断掉，而且不报错，只是永远说"没有找到清单文件"。

正确做法是 `SHGetFolderPathW`（读的就是资源管理器在用的值）：

```python
CSIDL_DESKTOPDIRECTORY = 0x0010   # 真实桌面
CSIDL_PROGRAMS         = 0x0002   # 当前用户的「开始菜单 → 程序」

buf = ctypes.create_unicode_buffer(1024)
ctypes.windll.shell32.SHGetFolderPathW(None, CSIDL_DESKTOPDIRECTORY, None, 0, buf)
```

PowerShell 侧则读注册表的 `User Shell Folders`（下载目录的键名是个 GUID）：
`{374DE290-123F-4565-9164-39C4925E467B}`。

同类目录（`Documents` / `Downloads` / `Pictures` / `AppData`）都可能被重定向，
**一律问系统，不要拼**。安装脚本还会提示「`%USERPROFILE%\Desktop`
与真实桌面不是同一目录」，免得用户以为没装成功。

**启动器的编码必须带 BOM。** Windows 自带的 5.1 版把「无 BOM 的 UTF-8」
按系统 ANSI 代码页解析 —— 简体中文环境下脚本里的中文会全部变乱码，
连路径都读不出来。`assets/add_bom.py` 负责补上，并抽样校验中文完好。

**快捷方式建完必须回读。** 只写不读的话，结构不对也不会报错 ——
用户看到的只是一个双击没反应的图标。`assets/make_shortcut.py` 用系统
Shell 的 IShellLink 接口创建，然后重新打开它、读回目标路径/参数/图标，
逐项比对。

**覆盖被拦时，先看现有内容对不对。** 有些环境会拦下「覆盖已有文件」。
此时直接报失败会误导人 —— 若现有的那份内容本来就正确，应当报成功。
（本机就遇到了：内容正确，却因为覆盖被拦而报 ✗。）

**卸载走回收站。** 产品自己主张「删除 = 移到可撤销的地方」，
卸载自己的组件时当然也得守这条规矩。用的是 `FOF_ALLOWUNDO`
（而非 `rmtree`），实测确认内容确实进了回收站。

**支持自动化。** `-Action survey -NoWait` 可无人值守跑完盘点；
`-NoWait` 下**一切确认动作默认"否"** —— 无人值守时最危险的就是替用户按了确认。

**不要用 `Start-Process` 起后台进程。** PowerShell 5.1 的 `Start-Process`
在构造环境块时用一个**大小写敏感**的字典存环境变量。本机同时存在
`http_proxy` 和 `HTTP_PROXY`（装了带代理的软件基本都会这样），于是它直接抛
`已添加项。字典中的关键字: http_proxy 所添加的关键字: HTTP_PROXY`。
改用进程对象自己拼一条 `cmd /c "..."` 来做重定向（`UseShellExecute = $false`）。

**后台启动也要能报告成败。** `Start-Process` 拿不到退出码，所以启动后
**等产物出现**（对比 HTML 的时间戳）才算成功；超时或进程异常退出时，
把错误输出的末尾几行打出来。否则失败了用户只会看到"已启动"。

---

## 其余命令

**只读体检** —— 出 Markdown 报告 + JSON：

- 并行扫描整个盘 / 任意目录，统计每个文件与目录的**逻辑大小和物理占用**
- 用规则库给每条路径**归因**：这是什么、能不能删、删了怎么恢复、该怎么删
- 采集**系统级隐形占用**：休眠文件、页面文件、卷影副本（其他工具扫不到的部分）

**核对单条判定** —— 不确定某项为什么被判成可删时：

```bash
diskdoctor explain "C:\Users\你\AppData\Local\Temp"
```

它把归因链路逐层摊开，包括"某条规则本可命中、但因语境约束主动退让"这种
最容易被误读为"规则库漏了"的情况。

---

## 清理的完整流程

```bash
# 1. 生成可浏览的界面，逐条看清装了什么、能不能删
diskdoctor survey "C:\Users\你" --open

# 2. 在界面里勾选 → 导出清单   （或者跳过界面，用下面的方式直接扫）

# 3. 预览会清理什么（不动任何文件）
diskdoctor clean --from-selection 导出的清单.json --dry-run

# 4. 执行 —— 文件进入暂存区（可撤销）
diskdoctor clean --from-selection 导出的清单.json --yes

# 5. 查看暂存区
diskdoctor trash list
diskdoctor trash usage

# 6a. 后悔了 → 文件回到原位置
diskdoctor trash restore --id <操作ID>

# 6b. 或确认无误 → 真正释放空间（不可逆，需二次确认）
diskdoctor trash purge --older-than 7 --yes
```

也可以不用界面、直接按目录扫（适合脚本场景）：

```bash
diskdoctor clean "C:\Users\你的用户名" --dry-run
```

### 核对单条为什么被判成可删

```bash
$ diskdoctor explain "C:\Users\me\.vscode\extensions\ms-python.python\out\client\node_modules"

逐层判定（每层的结论会被其下级继承）
  层级               命中规则           安全等级      优先级
  .vscode            （未命中）          —           —
  extensions         ide-extensions     需确认       96
  ms-python.python   （继承自上级）       需确认       96
  ...
  node_modules       （继承自上级）       需确认       96
     └─ 规则 `node-modules`（本可判为「可安全回收」）因语境约束**主动不适用**

最终结论
  归因规则 : ide-extensions （IDE 扩展目录）
  安全等级 : 需确认
```

它会把**语境约束的退让**也显式说出来 —— 否则用户看到"未命中"会以为规则库漏了，
而实际上是安全设计生效了。

### 清单为什么比实际条目短

`survey` 与 `clean` 都会把小条目折叠，避免"上百个几十字节的缓存"淹没真正的大头：

| 类别 | 处理 | 显示 |
|---|---|---|
| 大条目 | 正常处理 | 逐条列出完整路径 |
| 小条目 | 正常处理 | 聚合成一行「另有 N 项，合计 X」 |
| **用户文档区的小条目** | **排除，不清理** | 单独提示，说明已排除 |

第三类是刻意的：实测一次扫描里有 121 项挤在 `Documents` 下、合计仅 35 MB，
却占了清单 43% 的行数。**为几十 MB 去动上百个你自己的文件，
风险和收益完全不成比例。**

用 `--merge-below-mb 0`（clean）或 `--merge-below-kb 0`（survey）可关闭折叠。

### 为什么清理分成两步

**移进暂存区不会释放空间** —— 文件还在盘上，只是换了位置。必须 `purge` 才真正回收。

这是**刻意用空间换安全**：中间那一周就是留给你反悔的。想省事就用
`trash purge --older-than 7`，让超过 7 天的自动清掉。

### purge 之后，务必看它报的「实测释放量」

`purge` 不只说"删了多少"，还会说"磁盘**实际**多了多少" —— 这两个数字可能不一致，
而后者才是你关心的。工具会按情况给出不同解释：

```
C:\ 可用空间：25.27 GB → 25.27 GB（反而少了 128.0 KB）

⚠ C:\ 上这次删除**没有真正归还空间**（声称删除 400.0 MB）。
  最常见的原因是：本机的删除被重定向了 —— 文件进了**回收站**，
  仍然占着磁盘，直到清空回收站才会释放。
  某些环境（安全软件、企业策略、代理运行时）会这样拦截删除。
```

**为什么这个提示重要**：我们实测遇到过一种环境，所有删除操作
（包括 `rm`、`remove_dir_all`、构建工具清理产物）**都被自动重定向到回收站**。
在那种环境里，清理工具报"已释放 2.13 GB"是**错的** —— 空间一点没多。

这个功能就是用来兜住这类情况的：不假设、不谎报，每次实测，不符就解释。

### 回收站可能是最大的可回收项

体检报告的「可安全回收」里，回收站常常排在第一位（实测见过 4 GB → 10.5 GB）。

但**本工具不会替你清它** —— 手工搬动回收站内部文件会破坏它的索引结构
（`$I` 元数据与 `$R` 数据必须成对维护）。所以规则库里给它的是
`action: command`，只告诉你命令，不代你执行：

```powershell
Clear-RecycleBin -Force
```

> 顺带一提：在上面说的那种"删除被重定向"的环境里，**清空回收站是唯一
> 真正释放空间的操作**。清理工具移除的文件全都堆在那里。

### 暂存区在哪

每个卷的根目录下：`<盘符>:\.diskdoctor-trash`

**为什么不是固定在 C 盘**：同卷内移动才是一次瞬间的改名操作。如果暂存区固定在
C 盘，清理 D 盘时就变成跨盘复制 —— 想删 20 GB 得先有 20 GB 空闲空间，
还可能中途失败留下半份拷贝。放在被清理文件所在的卷上，就永远不需要额外空间。

代价是暂存区分散在各盘，用 `diskdoctor trash usage` 汇总查看。

---

## 安全设计

这个工具会移动你的文件，所以安全不是"尽力而为"，是硬约束。

### 两层防护，职责分明

| | 作用 | 特点 |
|---|---|---|
| **硬护栏**（`dd-exec/src/guard.rs`） | 挡死系统关键路径：Windows / Program Files / System Volume Information / 回收站 / hiberfil.sys / NTUSER.DAT | **硬编码，不读任何数据文件**，改不了也绕不过 |
| **规则库判定** | 挡 `protected` / `system-managed` 级内容，并用**语境约束**处理"看起来能删、实际是软件运行必需"的情况 | 知道 `node_modules` 在项目里可删、在软件安装目录里不可删 |

两层是「与」的关系，任一拒绝就不执行。

护栏刻意不依赖规则库 —— 规则库是可被修改的 YAML，而护栏必须永远有效。
它是「即使前面所有判断都错了、规则库被改坏了，也不会出事」的最后保证。

### 语境约束：`unless-below`

这一节记录一个**出过两次的同类事故**，值得单独说明：

| # | 路径 | 为什么危险 |
|---|---|---|
| 1 | `D:\Program\...\node_modules` | 自定义软件安装目录，那是软件自带的运行时依赖 |
| 2 | `.vscode\extensions\...\node_modules` | IDE 扩展自带的依赖，删了扩展直接失效 |

两次都是 `node_modules` 被当成"删了能重装"，而实际是**软件运行的必备文件**。

**靠调 priority 压不住** —— 那要求每新增一个 IDE / 应用目录，都得记得给它
设一个比 68 更高的优先级，迟早会漏。所以改用**加法的语境约束**：
声明一条"可重建"规则时，顺手说清它在哪些语境下不成立：

```yaml
- id: node-modules
  match: { kind: name, pattern: 'node_modules' }
  safety: safe
  priority: 68
  unless-below: &app_context        # 祖先路径含任一片段 → 本规则不适用
    - '\extensions\'                # IDE 扩展（.vscode/.qoder/.cursor 通用）
    - '\resources\app'              # Electron 应用资源
    - '\programs\'                  # 用户级程序安装目录
    - '\site-packages\'             # Python 第三方库
    ...
```

新增 IDE 目录时**不需要改这条规则**。

两条位于锚点定义之前的规则只能手工写全列表，
`validate_rules.py` 的「语境约束一致性」检查会保证它们不失同步。

> 顺带修掉的一个隐患：规则字段名写成 `unless_below`（下划线）而不是
> `unless-below`（连字符）时，serde 会**静默用默认值**，规则悄悄失效。
> 现在 `RawRule` 加了 `deny_unknown_fields`，拼错字段名会直接报错。

### 会挡住的典型情况

- 路径穿越：`C:\Users\..\Windows` 会先 canonicalize 解析成真实路径再判断，绕不过
- 暂存区自身：不能把暂存区里的东西再移进暂存区
- 盘符根：`C:\` 代表整个分区，不能整体移动
- 混合清单：清单里混进受保护项时**跳过它继续处理其余**，不会整批失败

### 断电安全（两阶段提交）

每个操作目录下都有一份 `manifest.json`，执行顺序是：

1. 写 manifest，状态 `pending`，**fsync 落盘**
2. 逐项 rename
3. 重写 manifest，状态 `staged`

第 2 步断电时，重启后能看到 `pending` 的完整计划，从而判断「哪些已移走、
哪些还没有」，不会出现"文件在暂存区但没人知道它从哪来"的孤儿。

任何一步失败都会**回滚已移动的部分**，宁可整批取消也不留半成品状态。

### 冲突处理

恢复时如果原位置已有同名文件，**跳过并报告**，不覆盖也不改名。
覆盖会丢你的新数据，改名会让你以为文件丢了 —— 跳过是最保守也最好解释的做法。

---

## 快速开始

```bash
# 1. 取到源码（克隆或解压本仓库），在仓库根目录执行
cargo build --release            # 产物：target/release/diskdoctor.exe

# 2. 体检整个 C 盘，报告写到文件
./target/release/diskdoctor.exe C:\ --out report.md --json result.json

# 3. 只看某个目录
./target/release/diskdoctor.exe "C:\Users\你的用户名" --out report.md

# 4. 规则库自检 / 查看全部规则
./target/release/diskdoctor.exe --check-rules
./target/release/diskdoctor.exe --list-rules

# 5. 生成可浏览的决策界面（推荐从这条开始用）
./target/release/diskdoctor.exe survey "C:\Users\你的用户名" --open
```

> **只支持 Windows。** 护栏清单和目录规则都建立在 Windows 的系统布局上
> （`Program Files`、`System Volume Information`、`$Recycle.Bin`、
> `hiberfil.sys`、卷影副本……），换到别的系统不成立。
>
> **需要 Rust**（stable 即可，本工程用 2021 edition）。没有的话见下面的
> 「Windows 构建环境」。

> 建议以**管理员身份**运行：否则 `$Recycle.Bin`、部分 `AppData`、
> 卷影副本等位置读不到，报告会标注「因权限跳过 N 处」。

### Windows 构建环境

**推荐：MSVC 工具链（标准路径）**

```powershell
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools
# 安装时勾选「使用 C++ 的桌面开发」；这一步需要管理员权限
rustup default stable-x86_64-pc-windows-msvc
```

**备选：GNU 工具链（不需要 MSVC，体积小得多）**

在没法装 MSVC（比如没有管理员权限）的机器上，可以走这条：

```bash
rustup toolchain install stable-x86_64-pc-windows-gnu --profile default
rustup default stable-x86_64-pc-windows-gnu
```

走 GNU 时有两个必须知道的坑，否则会以各种奇怪的报错失败：

1. **`dlltool` 不在 PATH 上**。它藏在
   `~/.rustup/toolchains/<tc>/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained/`，
   而且需要同级的上一层 `bin/` 里的 `libgcc_s_seh-1.dll`。两个目录都要加进 PATH：

   ```bash
   RB="$HOME/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin"
   export PATH="$RB:$RB/self-contained:$PATH"
   ```

2. **依赖树里不能有 `windows-sys`**。它的 build script 会现场调 `dlltool`
   生成 import library，在 GNU 下很容易挂。这就是本工程关掉
   `clap` 的 `color` 特性、并且用手工 FFI 代替 `windows-sys` 的原因，
   详见根 `Cargo.toml` 的注释。

**网络受限环境**：若 `static.crates.io` / `static.rust-lang.org` 拉不动，
可以改用国内镜像 —— 在仓库根建一份 `.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = "rsproxy"

[source.rsproxy]
registry = "https://rsproxy.cn/crates.io-index"
```

工具链本身走镜像则用环境变量：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
```

> 顺带一句：同一时间**只跑一个 rustup 进程**。并发安装会把工具链装成半残
> （缺 `lib/rustlib`，报 `can't find crate for core`），而且报错信息完全不指向真因。

---

## 工程结构

```
DiskDoctor/                    ← 仓库根目录
├── crates/            源码（Rust workspace 的成员都在这里）
│   ├── dd-core/       数据模型：条目、快照、类别、安全等级
│   ├── dd-rules/      归因规则引擎 + 规则库（rules/rules.yaml，211 条）
│   ├── dd-scanner/    采集层：并行遍历后端（+ MFT 后端占位）
│   ├── dd-analyze/    分析层 + 报告渲染
│   ├── dd-exec/       执行层：硬护栏 + 暂存区（两阶段提交）
│   └── dd-cli/        命令行入口（scan / survey / clean / trash / explain）
│       ├── survey.rs      内容盘点界面（自包含 HTML）
│       └── serve.rs       只监听本机的极简服务（让界面上的路径可点开）
├── assets/            安装与打包辅助（不进二进制）
│   ├── install.py         安装 / 卸载（含快捷方式，带回读验证）
│   ├── make_shortcut.py   创建 .lnk（调系统 IShellLink）
│   ├── make_icon.py       生成多尺寸图标
│   ├── add_bom.py         给启动器脚本补 UTF-8 BOM
│   └── DiskDoctor.ps1     桌面启动器（菜单式，串起 survey → 选择 → clean）
├── docs/              设计文档
├── scripts/           规则校验与界面回归检查（见「测试」一节）
│   └── _paths.py          统一的路径解析（见下）
├── Cargo.toml         workspace
└── LICENSE            MIT 许可证

**不进仓库的**（已在 `.gitignore` 中排除，都是运行时生成的）：

├── reports/           体检报告、原始数据、生成的界面
│                      ← 含使用者磁盘的真实文件名与目录结构，不提交
└── target/            构建产物（cargo build 生成，可随时删）
```

### 关于 `scripts/_paths.py`

检查脚本原先各自硬编码了绝对路径，**项目一换目录就全部失效**，
而且失败方式只是"找不到文件"，看不出是路径写死了。

现在所有路径都由 `_paths.py` 统一解析，并且**尽量自动探测**：

| 项 | 探测顺序 |
|---|---|
| 项目根 | 以 `_paths.py` 的位置为准（跟着项目走） |
| 可执行文件 | 项目内 `target/` → 已安装位置 → `PATH` |
| Node | 环境变量 → `PATH` → 常见安装目录（含版本号目录） |
| 测试沙盒 | 项目内 `.tmp/`（不污染用户目录） |

任何一项都能用环境变量覆盖（`DD_EXE` / `DD_NODE` / `DD_TMP` / `DD_SURVEY_HTML`），
方便在别的机器上跑。

自检：

```bash
python scripts/_paths.py
```

它会报告项目结构、规则库、界面文件、可执行文件、Node 各自的解析结果 ——
**依赖出问题时先跑这个**，一眼就知道是哪一项没找到。

### 分层与依赖方向

```
dd-cli ─┬─→ dd-analyze ─→ dd-rules ─→ dd-core
        ├─→ dd-scanner ──────────────→ dd-core
        └─→ dd-exec ─────────────────→ dd-core
```

`dd-core` 不依赖任何东西，因此它定义的模型可以被将来任何一个前端
（Tauri 界面、CLI、甚至服务端）复用。

`dd-exec` 也**不依赖 dd-rules** —— 这是有意的。执行层的护栏不能被
规则库的数据影响，两者由 `dd-cli` 在调用处组合起来。

### 测试

在项目根目录执行（`scripts/` 下的路径都已改为自动解析）：

```bash
cargo test --workspace                        # 71 个 Rust 测试
python scripts/validate_rules.py              # 146 个规则用例

# 界面（HTML/CSS/JS）
python scripts/check_survey_js.py             # JS 语法 + 结构
python scripts/check_survey_css.py            # CSS 变量/类名/id 一致性
python scripts/check_survey_runtime.py        # 在 Node 里真实执行页面 JS
python scripts/check_contrast.py              # 配色对比度（WCAG）
python scripts/check_serve.py                 # 本机服务：安全边界 + 健康检查
python scripts/check_path_open.py             # 点路径真的弹出窗口（会开窗口）

python scripts/_paths.py                      # 路径解析自检
```

**注意**：界面相关的检查需要先有一份生成的界面文件
（`reports/我的磁盘内容.html`）。没有的话先生成：

```bash
cargo build --release
target\release\diskdoctor.exe survey "C:\Users\<你的用户名>" ^
    --html reports\我的磁盘内容.html --data reports\my-disk.json --expand-over-gb 5
```

这些检查脚本会在找不到可执行文件时自动回退到**已安装的那份**
（`%LOCALAPPDATA%\Programs\DiskDoctor`），所以没构建过也能跑。

**为什么要给界面写这些检查。** 页面里的问题**几乎从不报错** ——
取不到元素、变量没定义、字太暗，表现都只是"某处看着不对"或"整页空白"。
而这一页有近 400 行渲染逻辑，靠肉眼在浏览器里翻是查不全的。

| 脚本 | 抓什么 | 实际抓到过的 |
|---|---|---|
| `check_survey_js.py` | JS 语法、关键结构、括号配平 | 结构缺失 |
| `check_survey_css.py` | 引用了未定义的 CSS 变量、JS 里拼错的类名、不存在的 id | `--border-hi` 引用了但从未定义（靠 fallback 兜着） |
| `check_survey_runtime.py` | **运行时异常** —— 用一个最小 DOM 桩在 Node 里真的把页面 JS 跑一遍 | 桩写错时立刻暴露（筛选集合恒空 → 0 行） |
| `check_contrast.py` | WCAG 对比度 | 次要文字只有 4.10:1（低于 AA）；主按钮白字只有 3.17:1 |
| `check_path_open.py` | 点路径是否真的弹出窗口（枚举系统窗口标题，不看返回值） | — |

`check_survey_runtime.py` 不依赖 jsdom（那要装包），只实现了页面实际用到的
DOM 成员 —— 好处是这个桩本身就是一份"这页到底依赖什么 DOM API"的清单。

安全测试每一条都对应一个真实场景，多数是「如果这里没挡住，会发生什么」：
路径穿越、原位置冲突时不覆盖、purge 后不可恢复、暂存区必须与被清理文件同卷……

`serve.rs`（12 个用例）锁住界面上"点路径打开文件夹"的安全边界：
- 伪造/缺失令牌被拒
- 路径按**分量**比前缀（`C:\root2` 不能冒充 `C:\root` 的子路径）
- `..` 逃逸、根外路径、不存在的路径一律拒绝
- URL 解码要处理中文、`%2F`，且**不能把裸 `+` 当空格**（路径里可能有真的 `+`）
- 空闲超时可配置，非法值要退回默认而不是崩掉

端到端另有两组实测（`--serve` 全流程 15 项、空闲退出 2 项），
验证的是真机行为而不是函数返回值 —— 比如"点一下要等 5 秒"这类
只有真跑才会暴露的问题。

`attribution.rs`（8 个用例）锁住归因一致性与大小口径：
- 扫描子目录时**上层语境不能丢**（`.workbuddy` 的约束要能传到深层）
- 约束也不能**过度传染**（普通项目里的 `node_modules` 仍应可回收）
- 不同扫描层级对同一条路径必须给出**相同判定**
- **区域大小 ≠ 目录大小**：前者只算归本规则管的部分，混用会让占比超过 100%

最后那条 `check_survey_js.py` 是必需的 —— **HTML 里的 JS 编译期查不出来**。
Rust 侧只管拼字符串，JS 写错了照样编译通过，只有浏览器打开才白屏。
这个脚本会抽出内嵌 JS 交给 `node --check` 做真正的语法解析，再查关键结构。
（实测踩过：一次无心的字符串替换把事件处理器截断成三份，编译毫无问题。）

---

## 几个关键设计决定

### 1. 逐层并行的 BFS，而不是递归

递归下降会爆栈，而且无法保证「父节点先于子节点写入条目表」。
改用逐层并行 BFS 之后，父节点下标恒小于子节点下标，
`Snapshot::rollup()` 只要**倒着扫一遍**就能算出所有目录的总量 ——
不需要递归、不需要额外索引、不需要树结构。见 `dd-scanner/src/walk.rs`。

### 2. 归因是"目录先判、文件按优先级继承"

每个文件有「自己的规则」（如 `*.log`）和「从父目录继承」（如 `node_modules`）
两个来源，**取 priority 更高的那个**。所以 `node_modules/x.log` 会被正确
归到「构建产物 / 可安全回收」，而不是「日志 / 需确认」。

### 3. 汇报的是"归因区域的根"，不是每个文件

`node_modules` 下面几万个文件只汇报成**一条**。用户要的是
「这几坨东西」，不是「这 30 万个文件」。

### 4. 零分配的大小写不敏感匹配

归因跑在百万级条目上，逐条 `to_lowercase()` 就是上百万次堆分配。
规则引擎内部用 `eq_ignore_ascii_case` 直接比较原串。

### 5. 只为稀疏/压缩文件求真实物理占用

每个文件都调一次 `GetCompressedFileSizeW` 会显著拖慢扫描。
只有稀疏文件和压缩文件才会明显偏离逻辑大小（vhdx 正好属于这类，
而它恰恰是用户最想看清的），所以只对它们额外求一次。

---

## 安全约定（写进代码的，不是口号）

- **P0 阶段没有任何删除能力**，报告末尾会明确写出「本次运行为只读分析」
- 规则库中 `protected` 级别的路径（WinSxS、System32、Program Files…）
  **永远不会**成为清理建议
- `system-managed` 级别的路径只给出**官方命令**，不给"直接删"
- 「不确定」的一律标 `caution`，绝不标 `safe` —— 干净的错误比诱人的错误好

---

## 待接入（按优先级）

| 项 | 位置 | 说明 |
|---|---|---|
| **NTFS MFT 直读** | `dd-scanner/src/ntfs.rs` | 秒级全盘扫描 + 硬链接去重，需要管理员权限才能验证；实现计划已写在文件头部注释里 |
| USN Journal 增量 | `dd-scanner/` | 首次全量后，后续扫描亚秒级 |
| 重复文件三级漏斗 | `dd-analyze/` | 大小 → 前缀哈希 → Blake3 全量 |
| 快照 diff 时间轴 | `dd-analyze/` | 「这个月谁涨了」 |
| Tauri 2 桌面界面 | `src-tauri/` | 体检报告首屏 + Canvas treemap |

### 已实现的安全能力

- 暂存区 + 两阶段提交 + 一键撤销（`dd-exec`）
- 硬护栏：系统关键路径永不可达（`dd-exec/src/guard.rs`）
- **实测释放量校验**：不轻信"删了多少"，而是实测"磁盘实际多了多少"，
  并能识别"删除被重定向到回收站"这类环境特性（`dd-exec/src/space.rs`）
- **幂等**：重复清理同一批次会被识别，不会重复声称释放量
- **尽力而为**：单项失败（被占用）不拖累其余，且逐项记录原因
- 51 个安全测试覆盖关键场景（`dd-exec/tests/`）

---

## 规则库

规则库在 `crates/dd-rules/rules/rules.yaml`，与代码解耦，
加规则只需要改 YAML 不用改 Rust。

规则**按书写顺序求值，首次命中即停止**，所以必须由具体到宽泛排列
（`appdata\local\npm-cache` 一定要排在 `appdata` 之前）。
需要打断顺序时用 `priority` 字段（数值大的先算）。

支持的匹配方式：

| kind | 说明 | 示例 |
|---|---|---|
| `name` | 名字精确匹配（忽略大小写） | `node_modules` |
| `name-glob` | 名字通配 | `thumbcache_*.db` |
| `ext` | 扩展名 | `log` |
| `path-suffix` | 相对路径结尾 | `appdata\local\temp` |
| `path-contains` | 相对路径包含 | `.cargo\registry\cache` |
| `path-glob` | 相对路径整体通配 | `appdata\roaming\code\*cache` |

> `path-glob` 不需要写前导 `*`，引擎会自动补，用来匹配前面还有用户名等层级的情况。
