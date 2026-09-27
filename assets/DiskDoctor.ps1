# DiskDoctor 桌面启动器
#
# 为什么需要这个脚本而不是直接给 exe 建快捷方式：
# 产品的主链路是「看清装了什么 → 判断能否删 → 自己挑 → 执行」。第三步在
# 浏览器里（勾选导出），第四步需要命令行 —— 中间隔着一次手工敲命令，
# 多数人到这里就断了。这个启动器把那一步补上：双击 → 选 → 跑。
#
# 用法：
#   DiskDoctor.ps1                        打开菜单
#   DiskDoctor.ps1 -Action survey         直接盘点
#   DiskDoctor.ps1 -Action clean          直接进入清理流程
#   DiskDoctor.ps1 -Action trash          直接看暂存区
#   DiskDoctor.ps1 -Action survey -Path D:\  换个位置盘点

param(
    [ValidateSet("menu", "survey", "clean", "trash")]
    [string]$Action = "menu",

    # 要盘点的位置，默认整个用户目录
    [string]$Path = $env:USERPROFILE,

    # 超过多少 GB 的条目在界面上可以展开看内部
    [int]$ExpandOverGb = 5,

    # 完成后不等待按键。
    #
    # 给自动化用：计划任务 / 脚本里调用时没人按键，等待会永远挂住。
    # 此时 -Action 必须显式给出（菜单必须交互）。
    [switch]$NoWait
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

$AppDir = $PSScriptRoot
$Exe = Join-Path $AppDir "diskdoctor.exe"
$OutDir = Join-Path $env:LOCALAPPDATA "DiskDoctor\out"

# ---------------------------------------------------------------- 基础

function Write-Head {
    Clear-Host
    Write-Host ""
    Write-Host "  DiskDoctor · 磁盘内容盘点" -ForegroundColor Cyan
    Write-Host ("  " + ("─" * 56)) -ForegroundColor DarkGray
    Write-Host ""
}

# 下面两个函数整行一次输出，不用 -NoNewline 拼色。
# 原因：PowerShell 5.1 把 -NoNewline 拆成独立记录，一旦输出被重定向到文件
# （日志、计划任务），符号和文字会各自占一行，看起来像坏了。
function Write-Err2 {
    param([string]$Msg)
    Write-Host "  ✗ $Msg" -ForegroundColor Red
}

function Write-Ok {
    param([string]$Msg)
    Write-Host "  ✓ $Msg" -ForegroundColor Green
}

# 等一个按键再继续。-NoWait 时直接返回，让整个脚本可用于自动化。
function Wait-Key {
    param([string]$Prompt = "  按回车返回菜单")
    if ($NoWait) { return }
    Read-Host $Prompt | Out-Null
}

function Assert-Exe {
    if (-not (Test-Path $Exe)) {
        Write-Host ""
        Write-Err2 "找不到 diskdoctor.exe"
        Write-Host "    期望位置: $Exe"
        Write-Host ""
        Write-Host "  说明：启动器需要和 diskdoctor.exe 放在同一目录。" -ForegroundColor DarkGray
        Write-Host "        如果你移动过文件，请把它们放回一起。" -ForegroundColor DarkGray
        Write-Host ""
        Wait-Key "  按回车关闭"
        exit 1
    }
}

# 起一个「脱离本窗口」的进程，并把输出重定向到文件。
#
# 为什么不用 Start-Process：PowerShell 5.1 的 Start-Process 在构造环境块时
# 用一个**大小写敏感**的字典存环境变量。本机环境里同时存在 `http_proxy`
# 和 `HTTP_PROXY`（很多代理工具都会这么设），于是它直接抛
#   "已添加项。字典中的关键字: http_proxy 所添加的关键字: HTTP_PROXY"
# —— 也就是说，用户机器上只要装了带代理的软件，Start-Process 就不能用了。
#
# 这里改用进程对象自己拼一条 cmd 命令来做重定向。UseShellExecute=false
# 让它不弹窗，`cmd /c "..."` 负责把 stdout/stderr 落到文件。
function Start-Detached {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkDir,
        [string]$OutFile,
        [string]$ErrFile
    )

    $quoted = ($Arguments | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "cmd.exe"
    $psi.Arguments = "/c `"`"$FilePath`" $quoted > `"$OutFile`" 2> `"$ErrFile`"`""
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.WorkingDirectory = $WorkDir

    return [System.Diagnostics.Process]::Start($psi)
}

# 找一个可写的输出目录。程序目录可能在 Program Files 下（只读），
# 所以统一写到 LOCALAPPDATA。
function Get-OutDir {
    if (-not (Test-Path $OutDir)) {
        New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
    }
    return $OutDir
}

# 取「已知文件夹」的真实位置。
#
# **绝对不能拼 $env:USERPROFILE** —— 桌面、文档、下载都可能被重定向到别的盘。
# 本机上桌面在 D:\桌面、下载在 D:\下载，而 C:\Users\...\Desktop 与
# C:\Users\...\Downloads 只是两个过时的空壳目录。拼出来的路径能写、也存在，
# 所以错了也不报错，只是东西永远到不了用户会去看的地方。
#
# 走注册表的 User Shell Folders（资源管理器用的就是这个值）。
# 用 Get-ItemProperty 而不是 COM，避免触发脚本安全策略。
function Get-KnownFolder {
    param([string]$Key)

    $reg = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders"
    try {
        $v = (Get-ItemProperty -Path $reg -Name $Key -ErrorAction Stop).$Key
        if ($v) { return [Environment]::ExpandEnvironmentVariables($v) }
    } catch {
        # 读不到就交给调用方决定退路
    }
    return $null
}

# 用户可能把「导出清单」存到哪几个地方。
#
# 浏览器导出走的是**下载**目录 —— 它是最可能的落点，所以要排在前面。
function Get-SearchDirs {
    $dirs = @()

    # 下载：注册表里是 GUID 键名
    $dl = Get-KnownFolder -Key "{374DE290-123F-4565-9164-39C4925E467B}"
    if ($dl) { $dirs += $dl }

    $dirs += $OutDir

    foreach ($k in @("Desktop", "Personal")) {
        $p = Get-KnownFolder -Key $k
        if ($p) { $dirs += $p }
    }

    # 退路：万一注册表读不到，至少还有默认位置
    foreach ($sub in @("Downloads", "Desktop", "Documents")) {
        $p = Join-Path $env:USERPROFILE $sub
        if ((Test-Path $p) -and ($dirs -notcontains $p)) { $dirs += $p }
    }

    return $dirs | Select-Object -Unique
}

# ---------------------------------------------------------------- 盘点

function Invoke-Survey {
    param([string]$Target)

    Assert-Exe
    if (-not (Test-Path $Target)) {
        Write-Host ""
        Write-Err2 "路径不存在: $Target"
        Write-Host ""
        return
    }

    $dir = Get-OutDir
    $html = Join-Path $dir "磁盘内容.html"
    $json = Join-Path $dir "磁盘内容.json"
    $svcOut = Join-Path $dir "server.out.log"
    $svcErr = Join-Path $dir "server.err.log"

    Write-Host "  扫描位置 : $Target"
    Write-Host "  输出到   : $html"
    Write-Host ""
    Write-Host "  正在扫描 …（大目录可能要十几秒）" -ForegroundColor DarkGray
    Write-Host ""

    # 记下旧文件时间戳：用来判断这次扫描是否真的产出了新结果
    $oldStamp = 0
    if (Test-Path $html) { $oldStamp = (Get-Item $html).LastWriteTime.Ticks }

    # 在**独立的后台进程**里跑。
    #
    # 为什么要后台：--serve 会让命令常驻（等界面上的路径点击），前台跑的话
    # 菜单会卡在这里，用户点不到 [2]。用独立进程 + 隐藏窗口，服务随时可用，
    # 而且 10 分钟无操作会自己退出，不会留下孤儿进程。
    $argList = @(
        "survey", $Target,
        "--html", $html,
        "--data", $json,
        "--expand-over-gb", $ExpandOverGb,
        "--serve",      # 界面上的路径可以点开
        "--open"        # 扫描完自动开浏览器
    )

    $proc = $null
    try {
        $proc = Start-Detached -FilePath $Exe -Arguments $argList `
            -WorkDir $AppDir -OutFile $svcOut -ErrFile $svcErr
    } catch {
        Write-Err2 "无法启动扫描进程：$($_.Exception.Message)"
        Write-Host ""
        Wait-Key
        return
    }

    # 等它把新界面写出来。
    #
    # 后台启动拿不到退出码，所以这里用「产物是否更新」来判断成败 ——
    # 比盲目报"已启动"可靠。
    $deadline = (Get-Date).AddMinutes(6)
    $ok = $false
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 400

        $stamp = 0
        if (Test-Path $html) { $stamp = (Get-Item $html).LastWriteTime.Ticks }
        if ($stamp -ne $oldStamp) { $ok = $true; break }

        if ($proc -and $proc.HasExited -and $proc.ExitCode -ne 0) { break }
    }

    Write-Host ""
    if (-not $ok) {
        Write-Err2 "扫描没有产出结果"
        if ($proc -and $proc.HasExited) {
            Write-Host "    进程已退出，代码 $($proc.ExitCode)" -ForegroundColor DarkGray
        }
        if (Test-Path $svcErr) {
            $tail = (Get-Content $svcErr -Tail 8 -ErrorAction SilentlyContinue) -join "`n"
            if ($tail) {
                Write-Host ""
                Write-Host "  错误输出：" -ForegroundColor DarkGray
                Write-Host "    $tail" -ForegroundColor DarkGray
            }
        }
        Write-Host ""
        Wait-Key
        return
    }

    Write-Ok "界面已生成，浏览器正在打开"
    Write-Host ""
    Write-Host "  在浏览器里：" -ForegroundColor DarkGray
    Write-Host "    · 逐条看清每个目录/文件是什么、能不能删" -ForegroundColor DarkGray
    Write-Host "    · 大块（超过 $ExpandOverGb GB）点开看里面装了什么" -ForegroundColor DarkGray
    Write-Host "    · **点任意路径可以直接在资源管理器中打开它**" -ForegroundColor DarkGray
    Write-Host "    · 勾选你要清理的，点「导出清单」" -ForegroundColor DarkGray
    Write-Host "    · 回来选菜单 [2] 执行" -ForegroundColor DarkGray
    Write-Host ""
    Write-Host "  路径跳转服务已在后台运行（10 分钟无操作自动停止）。" -ForegroundColor DarkGray
    Write-Host "  如果想现在就改用无服务的静态界面，关掉那个进程即可。" -ForegroundColor DarkGray
    Write-Host ""

    Wait-Key
}

# ---------------------------------------------------------------- 执行清理

# 收集最近的清单文件。界面导出的文件名形如 diskdoctor-selection-<时间>.json，
# 浏览器默认存到「下载」，所以同时看好几个常见位置。
function Find-Selections {
    $places = (Get-SearchDirs) | Where-Object { Test-Path $_ }

    $found = @()
    foreach ($p in $places) {
        $found += Get-ChildItem -Path $p -Filter "diskdoctor-selection-*.json" -File -ErrorAction SilentlyContinue
    }
    # 也认一下手工改名的
    foreach ($p in $places) {
        $found += Get-ChildItem -Path $p -Filter "selection*.json" -File -ErrorAction SilentlyContinue
    }

    return $found | Sort-Object LastWriteTime -Descending | Select-Object -Unique -First 8
}

function Invoke-Clean {
    Assert-Exe

    $list = @(Find-Selections)
    if ($list.Count -eq 0) {
        Write-Host ""
        Write-Host "  没有找到清单文件。" -ForegroundColor Yellow
        Write-Host ""
        Write-Host "  先做这一步：" -ForegroundColor DarkGray
        Write-Host "    1. 用菜单 [1] 生成界面" -ForegroundColor DarkGray
        Write-Host "    2. 在界面里勾选要清理的" -ForegroundColor DarkGray
        Write-Host "    3. 点「导出清单」—— 浏览器会把它存到「下载」" -ForegroundColor DarkGray
        Write-Host ""
        Wait-Key
        return
    }

    Write-Host "  找到这些清单（最近的在前）："
    Write-Host ""
    for ($i = 0; $i -lt $list.Count; $i++) {
        $f = $list[$i]
        $mb = ""
        try { $mb = "  ({0:N0} KB)" -f ($f.Length / 1KB) } catch { }
        Write-Host ("    [{0}] {1}" -f ($i + 1), $f.Name)
        Write-Host ("        {0}{1}" -f $f.DirectoryName, $mb) -ForegroundColor DarkGray
        Write-Host ("        {0}" -f $f.LastWriteTime) -ForegroundColor DarkGray
    }
    Write-Host ""
    Write-Host "    [0] 返回" -ForegroundColor DarkGray
    Write-Host ""

    $pick = ""
    if ($NoWait) {
        # 自动化时不问，取最近一份 —— 然后仍然只做试运行（见下方确认步骤）
        $pick = "1"
        Write-Host "  （自动化模式：自动选择最近一份清单）" -ForegroundColor DarkGray
    } else {
        $pick = Read-Host "  选择要执行的清单编号"
    }
    if ($pick -eq "0" -or $pick -eq "") { return }

    $idx = 0
    if (-not [int]::TryParse($pick, [ref]$idx)) {
        Write-Err2 "请输入编号"; Start-Sleep -Seconds 1; return
    }
    if ($idx -lt 1 -or $idx -gt $list.Count) {
        Write-Err2 "编号超出范围"; Start-Sleep -Seconds 1; return
    }
    $sel = $list[$idx - 1]

    # ---- 第一步：试运行 ----
    Write-Host ""
    Write-Host "  ── 试运行（不会动任何文件）──" -ForegroundColor DarkGray
    Write-Host ""
    & $Exe clean --from-selection $sel.FullName --dry-run
    $code = $LASTEXITCODE
    Write-Host ""

    if ($code -ne 0) {
        Write-Err2 "试运行失败（退出码 $code）"
        Write-Host ""
        Wait-Key
        return
    }

    # ---- 第二步：确认真执行 ----
    Write-Host "  说明：执行 = 把文件**移入暂存区**，不是删除。" -ForegroundColor DarkGray
    Write-Host "        空间不会立刻释放，但随时可以一键还原。" -ForegroundColor DarkGray
    Write-Host ""

    # -NoWait（自动化）下不去猜用户意图，默认**不执行**。
    # 无人值守时最危险的就是"替用户按了确认"。
    if ($NoWait) {
        Write-Host "  （自动化模式：只做了试运行，未执行。去掉 -NoWait 后交互确认。）" -ForegroundColor DarkGray
        Write-Host ""
        return
    }

    $ans = Read-Host "  确认执行？输入 y 执行，其它键取消"

    if ($ans -ne "y" -and $ans -ne "Y") {
        Write-Host ""
        Write-Host "  已取消，什么都没动。" -ForegroundColor DarkGray
        Write-Host ""
        Wait-Key
        return
    }

    Write-Host ""
    & $Exe clean --from-selection $sel.FullName --yes
    Write-Host ""
    Write-Host "  执行完毕。" -ForegroundColor DarkGray
    Write-Host ""
    Wait-Key
}

# ---------------------------------------------------------------- 暂存区

function Invoke-Trash {
    Assert-Exe
    Write-Host "  ── 暂存区 ──" -ForegroundColor DarkGray
    Write-Host ""
    & $Exe trash list
    Write-Host ""
    & $Exe trash usage
    Write-Host ""

    if ($NoWait) {
        Write-Host "  （自动化模式：不自动还原。去掉 -NoWait 后交互操作。）" -ForegroundColor DarkGray
        Write-Host ""
        return
    }

    $ans = Read-Host "  要还原最近一批吗？输入 y 还原，其它键返回"
    if ($ans -ne "y" -and $ans -ne "Y") { return }

    & $Exe trash restore
    Write-Host ""
    Wait-Key
}

# ---------------------------------------------------------------- 菜单

function Show-Menu {
    while ($true) {
        Write-Head
        Write-Host ("  当前默认扫描位置: " + $Path) -ForegroundColor DarkGray
        Write-Host ""
        Write-Host "    [1] 盘点磁盘内容" -ForegroundColor Cyan
        Write-Host "        生成界面，看清每个目录是什么、能不能删" -ForegroundColor DarkGray
        Write-Host ""
        Write-Host "    [2] 执行清理" -ForegroundColor Cyan
        Write-Host "        用你在界面里勾选导出的清单跑（先试运行，再确认）" -ForegroundColor DarkGray
        Write-Host ""
        Write-Host "    [3] 查看 / 还原暂存区" -ForegroundColor Cyan
        Write-Host "        清理过的东西都在这里，可以整体撤回来" -ForegroundColor DarkGray
        Write-Host ""
        Write-Host "    [9] 换个扫描位置" -ForegroundColor Cyan
        Write-Host ""
        Write-Host "    [0] 退出" -ForegroundColor DarkGray
        Write-Host ""

        $c = Read-Host "  请选择"

        switch ($c) {
            "1" { Invoke-Survey -Target $Path }
            "2" { Invoke-Clean }
            "3" { Invoke-Trash }
            "9" {
                Write-Host ""
                $np = Read-Host "  输入要盘点的路径（例如 D:\ 或 D:\Program）"
                if ($np -ne "" -and (Test-Path $np)) {
                    $Path = $np
                    Write-Ok "已切换"
                } else {
                    Write-Err2 "路径无效，未改变"
                }
                Start-Sleep -Milliseconds 800
            }
            "0" { return }
            default { }
        }
    }
}

switch ($Action) {
    "survey" { Write-Head; Invoke-Survey -Target $Path }
    "clean" { Write-Head; Invoke-Clean }
    "trash" { Write-Head; Invoke-Trash }
    default { Show-Menu }
}
