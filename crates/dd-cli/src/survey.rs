//! `diskdoctor survey` —— 把磁盘内容变成一份可浏览、可挑选的决策界面。
//!
//! # 为什么需要它
//!
//! 在此之前，工具的产物是**打印在终端里的报告**。问题是：一次扫描动辄几百条，
//! 用户读不完，也没法逐条挑。`clean` 命令是"我判定完，你全盘接受"。
//!
//! 但用户的真实诉求是三段：**看清装了什么 → 判断能不能删 → 自己决定删哪些**。
//! 中间那两段需要的是**界面**，不是文本。
//!
//! 所以这里生成一个自包含的 HTML：数据内嵌，双击就能在浏览器里打开，
//! 不需要装任何东西、不需要起服务、不联网。
//!
//! # 为什么是 HTML 而不是桌面应用
//!
//! 浏览器本身就是最成熟的"列表 + 筛选 + 勾选"运行时。为了这个界面去引入
//! Tauri（几百 MB 的工具链、跨平台打包、签名）性价比很低。而 HTML 还有个
//! 好处：**这份清单可以直接存档和分享** —— 你三个月后想回顾当时删了什么，
//! 文件还在。
//!
//! # 安全边界
//!
//! 界面只负责"让你挑"，**它自己不删任何东西**。导出的是**意图清单**，
//! 执行要走 `clean --from-selection`，那一步会重新过护栏与规则判定。
//! 这样即使清单被手工编辑过（例如塞进 `C:\Windows`），也拦得住。

use anyhow::{Context, Result};
use dd_analyze::{human_size, Analysis};
use dd_core::{Action, Safety};
use dd_rules::Ruleset;
use dd_scanner::ScanOptions;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct SurveyArgs {
    /// 要盘点的路径
    pub path: String,

    /// 界面输出位置（默认写到当前目录）
    ///
    /// 刻意不叫 `--out`：顶层的 `--out` 是给报告的全局选项，
    /// 子命令用同名参数会和它冲突。
    #[arg(long, value_name = "FILE")]
    pub html: Option<PathBuf>,

    /// 同时导出一份原始数据（JSON，便于脚本处理）
    #[arg(long = "data", value_name = "FILE")]
    pub data: Option<PathBuf>,

    /// 完成后直接打开浏览器
    #[arg(long)]
    pub open: bool,

    /// 单条体积小于此值（KB）的直接折叠，不逐条列出
    ///
    /// 默认 64 KB：几百条几十字节的缓存会淹没真正的大头，
    /// 而它们加起来也省不下什么。折叠的条目会在页面里聚合显示。
    #[arg(long, value_name = "KB", default_value_t = 64)]
    pub merge_below_kb: u64,

    /// 超过这个体积（GB）的条目会被**逐层细分**，展示里面装了什么
    ///
    /// 存在的理由：`AppData\Local` 这类聚合条目在界面上只是"18 GB / 需确认"
    /// 一行 —— 用户知道它大，却不知道里面是什么，也就无从判断。
    /// 细分后能展开看到子目录的体积与各自判定。
    ///
    /// 设为 0 可关闭细分。
    #[arg(long, value_name = "GB", default_value_t = 5)]
    pub expand_over_gb: u64,

    /// 每个大条目最多展开多少个子项（按体积取前 N）
    #[arg(long, value_name = "N", default_value_t = 40)]
    pub expand_limit: usize,

    /// 允许在界面上点击路径、直接在资源管理器中打开该位置
    ///
    /// 浏览器不允许静态页面打开本地文件夹，所以这个能力需要一个只监听
    /// 本机的小服务代劳。开启后本命令会**常驻**，直到你按 Ctrl+C
    /// 或 10 分钟没有操作后自动退出。
    ///
    /// 服务只绑 `127.0.0.1`、只接受扫描范围内的路径、只做"打开/定位"
    /// 两种只读动作 —— 它没有任何删除或修改文件的能力。
    #[arg(long)]
    pub serve: bool,
}

// ------------------------------------------------------------------ 传给前端的结构

#[derive(Serialize)]
struct SurveyData {
    root: String,
    scanned_at: String,
    file_count: u64,
    dir_count: u64,
    /// 各安全等级的条目数与体积
    summary: Vec<SummaryRow>,
    /// 规则库没覆盖的体积（这部分需要人工判断，页面要显式提示）
    unattributed: u64,
    /// 被折叠的小条目（体积合计，条目数）
    folded_count: usize,
    folded_bytes: u64,
    items: Vec<SurveyItem>,
}

#[derive(Serialize)]
struct SummaryRow {
    safety: String,
    label: String,
    count: usize,
    /// 其中有多少条能在界面上勾选（移入暂存区）
    selectable_count: usize,
    bytes: u64,
}

#[derive(Serialize)]
struct SurveyItem {
    path: String,
    name: String,
    size: u64,
    is_dir: bool,
    file_count: u64,
    category: String,
    category_label: String,
    safety: String,
    safety_label: String,
    score: u8,
    rule_id: String,
    rule_name: String,
    why: String,
    recovery: String,
    /// 该怎么处理（已经是人类可读的文本）
    action: String,
    /// 处置方式的机器标识：`delete` / `command` / `compact` / `review` / `none`
    action_kind: String,
    /// 若处置方式是官方命令，这里给出命令文本
    action_command: Option<String>,
    /// 能不能在界面上勾选
    selectable: bool,
    inherited: bool,
    mtime: i64,
    /// 逐层细分：仅对超过阈值的条目填充，展示"里面装了什么"
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<SubItem>,
    /// 该目录在快照里的**真实总大小**，仅当它大于 `size` 时才有值。
    ///
    /// 为什么会有差额：`size` 是**归因区域**的大小 —— 只统计"归本规则管"
    /// 的那部分，归属其他规则的子区域被排除在外（这样各区域互不重叠、
    /// 加总恰好等于已归因总量，账才对得上）。
    ///
    /// 但用户看细分时的心理模型是"这个文件夹一共占多少"，
    /// 拿区域大小当分母会让子项占比之和超过 100%。所以细分时改用真实总大小，
    /// 并在界面上说明差额去哪了。
    #[serde(skip_serializing_if = "Option::is_none")]
    dir_total: Option<u64>,
}

/// 大条目的子项。比 `SurveyItem` 轻 —— 它只需要回答
/// "这一块是什么、多大、能不能删"，不需要完整的理由与恢复说明
/// （那些可以点进对应条目去看，或者本来就是同一套规则）。
#[derive(Serialize, Clone)]
struct SubItem {
    name: String,
    path: String,
    size: u64,
    /// 占父条目体积的百分比（保留一位小数）
    share: f64,
    is_dir: bool,
    file_count: u64,
    safety: String,
    safety_label: String,
    rule_id: String,
    rule_name: String,
    category_label: String,
    action_kind: String,
    action_command: Option<String>,
    selectable: bool,
    /// 判定与父条目不同 —— 这是个重要信号："这里有个不一样的东西"
    differs_from_parent: bool,
    /// 还能继续展开（它自己也超阈值且是目录）
    expandable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<SubItem>,
}

pub fn run(args: &SurveyArgs, rules: &Ruleset, opts: &ScanOptions) -> Result<()> {
    let root = std::fs::canonicalize(&args.path)
        .with_context(|| format!("路径不存在或无法访问: {}", args.path))?;
    let root = crate::strip_verbatim(root);

    println!("DiskDoctor · 内容盘点");
    println!("扫描路径 : {}", root.display());
    println!("开始扫描 …");

    let t0 = std::time::Instant::now();
    // 采集系统级特殊项：它们是"隐形占用"，用户看不到但确实占地方，
    // 理应出现在"我的磁盘里装了什么"的答案里
    let snapshot = dd_scanner::scan_with_specials(&root, opts).context("扫描失败")?;
    let scan_ms = t0.elapsed().as_millis();
    let file_count = snapshot.file_count();
    let dir_count = snapshot.dir_count();

    println!(
        "扫描完成 : {} 个文件 / {} 个目录，用时 {:.1}s",
        file_count,
        dir_count,
        scan_ms as f64 / 1000.0
    );

    let analysis = Analysis::run(snapshot, rules.clone(), "并行目录遍历");

    // —— 建子节点索引 ——
    //
    // 快照只有 parent → 反向指针，没有 children。细分需要"往下看一层"，
    // 所以要自己建一次索引。只建一次、供所有大条目复用，
    // 否则每个大条目都遍历全表会变成 O(条目数 × 大条目数)。
    let child_index = build_child_index(&analysis.snapshot);

    // —— 组装给前端的数据 ——
    let merge_below = args.merge_below_kb.saturating_mul(1024);
    let expand_over = args.expand_over_gb.saturating_mul(1024 * 1024 * 1024);
    let mut items: Vec<SurveyItem> = Vec::new();
    let mut folded_count = 0usize;
    let mut folded_bytes = 0u64;
    // 注意：这是**剩余预算**，初值必须是 NODE_BUDGET。
    // 曾经把它当"已用计数"从 0 开始传进去，而 expand_subtree 第一行就是
    // `if *budget == 0 { return }` —— 于是永远立即返回，细分静默失效。
    // 编译器其实给了线索（`NODE_BUDGET is never used`），但当时没跟。
    let mut budget = NODE_BUDGET;

    for f in &analysis.findings {
        // 折叠极小的条目：它们数量巨大但体积微不足道，逐条列出只会淹没重点。
        // 注意折叠**不影响能否被清理** —— 只是不在界面上逐条展示，
        // 汇总行会如实告知折了多少、共多少体积。想看全部就调 --merge-below-kb 0。
        if merge_below > 0 && f.size < merge_below && f.safety != Safety::Safe {
            folded_count += 1;
            folded_bytes += f.size;
            continue;
        }
        let (action_kind, action_command) = action_of(&f.action);

        // 目录在快照里的真实总大小。
        //
        // 它可能大于 `f.size`（区域大小）：区域只统计"归本规则管"的那部分，
        // 归属其他规则的子区域被排除在外。用户关心的是"这个文件夹一共多大"，
        // 所以阈值判断与占比计算都要用真实总大小 ——
        // 否则会出现"目录实际 1.2 GB 却不展开"这种莫名其妙的行为。
        let dir_real = if f.is_dir {
            child_index
                .by_path
                .get(&f.path.to_ascii_lowercase())
                .and_then(|i| analysis.snapshot.entries.get(*i as usize))
                .map(|e| e.size)
                .unwrap_or(f.size)
        } else {
            f.size
        };
        let dir_total = if dir_real > f.size {
            Some(dir_real)
        } else {
            None
        };

        // 大条目：往下细分，让用户看清"里面装了什么"。
        // 阈值以**目录真实大小**为准。
        let children = if expand_over > 0 && dir_real >= expand_over && f.is_dir {
            let n = expand_subtree(
                &analysis,
                &child_index,
                &f.path,
                f.safety,
                expand_over,
                args.expand_limit,
                0,
                &mut budget,
                dir_real,
            );
            n
        } else {
            Vec::new()
        };

        items.push(SurveyItem {
            path: f.path.clone(),
            name: f.name.clone(),
            size: f.size,
            is_dir: f.is_dir,
            file_count: f.file_count,
            category: f.category.key().to_string(),
            category_label: f.category_label.clone(),
            safety: f.safety.key().to_string(),
            safety_label: f.safety_label.clone(),
            score: f.score,
            rule_id: f.rule_id.clone(),
            rule_name: f.rule_name.clone(),
            why: f.why.clone(),
            recovery: f.recovery.clone(),
            action: f.action_hint(),
            selectable: is_selectable(f),
            action_kind: action_kind.to_string(),
            action_command,
            inherited: f.inherited,
            mtime: f.mtime,
            children,
            dir_total,
        });
    }

    // 按体积降序 —— 用户问"装了什么"，通常是想知道谁在占地方
    items.sort_by(|a, b| b.size.cmp(&a.size));

    let summary = build_summary(&items);

    let data = SurveyData {
        root: root.to_string_lossy().to_string(),
        scanned_at: now_text(),
        file_count,
        dir_count,
        summary,
        unattributed: analysis.unattributed,
        folded_count,
        folded_bytes,
        items,
    };

    // —— 渲染 HTML ——
    //
    // 服务必须在生成 HTML **之前**绑定：页面里要写进端口和令牌。
    let server = if args.serve {
        match crate::serve::Server::bind(&root) {
            Ok(s) => Some(s),
            Err(e) => {
                // 服务起不来不该让整个命令失败 —— 降级成静态页面，
                // 界面上的路径仍然可点（只能复制路径），只是不能直接打开。
                eprintln!("  ⚠ 无法启动本地服务（{e}），界面将退化为静态模式。");
                None
            }
        }
    } else {
        None
    };

    let json = serde_json::to_string(&data)?;
    // 防止内容里的 `</script>` 提前闭合脚本块
    let json_safe = json.replace('<', "\\u003c");
    let server_js = match &server {
        Some(s) => format!(
            "{{\"port\":{},\"token\":\"{}\"}}",
            s.port(),
            s.token()
        ),
        None => "null".to_string(),
    };
    let html = TEMPLATE
        .replace("__SERVER__", &server_js)
        .replace("__DATA__", &json_safe);

    let out_path = args.html.clone().unwrap_or_else(|| {
        let mut p = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        p.push(format!("diskdoctor-{}.html", safe_stem(&root)));
        p
    });
    std::fs::write(&out_path, &html)
        .with_context(|| format!("写入 {} 失败", out_path.display()))?;

    if let Some(jp) = &args.data {
        std::fs::write(jp, serde_json::to_string_pretty(&data)?)
            .with_context(|| format!("写入 {} 失败", jp.display()))?;
    }

    let selectable_bytes: u64 = data
        .items
        .iter()
        .filter(|i| i.selectable)
        .map(|i| i.size)
        .sum();
    let selectable_n = data.items.iter().filter(|i| i.selectable).count();
    let expanded_top = data.items.iter().filter(|i| !i.children.is_empty()).count();
    let expanded_nodes = NODE_BUDGET.saturating_sub(budget);

    println!();
    println!("界面已生成 : {}", out_path.display());
    if let Some(jp) = &args.data {
        println!("JSON 已生成: {}", jp.display());
    }
    println!();
    println!(
        "可挑选的条目 : {} 项，共 {}",
        selectable_n,
        human_size(selectable_bytes)
    );
    if expanded_top > 0 {
        println!(
            "大条目细分   : {} 个条目可展开看内部（共 {} 个子项）",
            expanded_top, expanded_nodes
        );
    }
    if data.folded_count > 0 {
        println!(
            "已折叠小条目 : {} 项（合计 {}，多为几十字节的缓存）",
            data.folded_count,
            human_size(data.folded_bytes)
        );
    }
    if data.unattributed > 0 {
        println!(
            "规则库未覆盖 : {} （这部分需要你自己判断）",
            human_size(data.unattributed)
        );
    }
    println!();
    println!("在浏览器里打开它，逐条勾选你要清理的，然后导出清单：");
    println!("  diskdoctor clean --from-selection <导出的文件> --dry-run");
    println!();
    println!("本命令只做只读扫描，没有删除或修改任何文件。");

    // 先讲清楚"路径可以点"，再打开浏览器 —— 否则用户看到页面时已经
    // 忘了终端里说过什么。
    if let Some(s) = &server {
        println!();
        println!("路径可点击 : 在界面上点任意路径，即可在资源管理器中打开它");
        println!("本地服务   : {}（仅本机可访问）", s.url());
        println!(
            "服务范围   : 只在 {} 之内，且只能「打开 / 定位」",
            root.display()
        );
        println!(
            "保持运行   : 关闭本窗口即停止；默认 {} 分钟无操作也会自动停止",
            crate::serve::DEFAULT_IDLE_MINUTES
        );
    }

    if args.open {
        open_in_browser(&out_path);
    }

    // —— 常驻服务 ——
    //
    // 放在最后：界面上"点路径打开文件夹"需要它活着。
    if let Some(s) = server {
        let reason = s.run();
        println!();
        println!("服务已停止：{reason}");
    }

    Ok(())
}

/// 从 `Action` 取出机器标识与命令文本。
fn action_of(a: &Action) -> (&'static str, Option<String>) {
    match a {
        Action::Delete => ("delete", None),
        Action::Command { cmd } => ("command", Some(cmd.clone())),
        Action::Compact => ("compact", None),
        Action::Review => ("review", None),
        Action::None => ("none", None),
    }
}

/// 建「父 → 子下标列表」索引，以及「绝对路径(小写) → 下标」映射。
///
/// 后者用于从 `Finding`（只有路径字符串）定位到快照条目。
fn build_child_index(snap: &dd_core::Snapshot) -> ChildIndex {
    let n = snap.entries.len();
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut by_path: std::collections::HashMap<String, u32> =
        std::collections::HashMap::with_capacity(n);

    for (i, e) in snap.entries.iter().enumerate() {
        let idx = i as u32;
        if e.parent != dd_core::ROOT_PARENT && (e.parent as usize) < n {
            children[e.parent as usize].push(idx);
        }
        by_path.insert(snap.abs_path(idx).to_ascii_lowercase(), idx);
    }

    // 每个目录下的文件数（递归含子目录里的）。
    //
    // 做法：反向遍历一次、把每个非目录条目的计数累加到父节点。
    // 正确性依赖「父的下标一定小于子」—— 扫描器是深度优先展开的，
    // 父条目先入表，所以这个前提成立（`finalize` 的体积累加也基于同一前提）。
    // 这样是 O(n)，比逐个节点做子树遍历便宜得多。
    let mut files_of: std::collections::HashMap<u32, u64> =
        std::collections::HashMap::new();
    for i in (0..n).rev() {
        let e = &snap.entries[i];
        let own = if e.is_dir { 0 } else { 1 };
        let from_kids = files_of.get(&(i as u32)).copied().unwrap_or(0);
        let total = own + from_kids;
        if e.is_dir {
            files_of.insert(i as u32, total);
        }
        if e.parent != dd_core::ROOT_PARENT && (e.parent as usize) < n {
            *files_of.entry(e.parent).or_insert(0) += total;
        }
    }

    ChildIndex {
        children,
        by_path,
        files_of,
    }
}

struct ChildIndex {
    children: Vec<Vec<u32>>,
    by_path: std::collections::HashMap<String, u32>,
    files_of: std::collections::HashMap<u32, u64>,
}

/// 细分一个目录：列出直接子项，并对仍然超过阈值的子项递归。
///
/// # 边界控制
///
/// - `MAX_DEPTH`：防止异常深的目录树把数据撑爆
/// - `NODE_BUDGET`：全局节点上限，超出后停止细分（页面里会说明）
/// - 每个父条目最多 `limit` 个子项，按体积取前 N —— 剩下的聚合说明
///
/// 这些上限是必要的：真实盘上有几十万目录，无限细分会产出几百 MB 的 HTML，
/// 那还不如不做。
const MAX_DEPTH: usize = 6;
const NODE_BUDGET: usize = 4000;

#[allow(clippy::too_many_arguments)]
fn expand_subtree(
    analysis: &Analysis,
    index: &ChildIndex,
    parent_path: &str,
    parent_safety: Safety,
    threshold: u64,
    limit: usize,
    depth: usize,
    budget: &mut usize,
    // 分母用**目录真实总大小**（而非区域大小），否则子项占比之和会超过 100%
    parent_total: u64,
) -> Vec<SubItem> {
    if depth >= MAX_DEPTH || *budget == 0 {
        return Vec::new();
    }

    let key = parent_path.to_ascii_lowercase();
    let Some(&idx) = index.by_path.get(&key) else {
        return Vec::new();
    };
    let Some(kids) = index.children.get(idx as usize) else {
        return Vec::new();
    };
    if kids.is_empty() {
        return Vec::new();
    }

    // 先按体积降序取前 limit 个
    let mut sorted: Vec<u32> = kids.clone();
    sorted.sort_by(|a, b| {
        analysis.snapshot.entries[*b as usize]
            .size
            .cmp(&analysis.snapshot.entries[*a as usize].size)
    });
    let total_kids = sorted.len();
    sorted.truncate(limit);

    let parent_size = parent_total.max(1);
    let mut out = Vec::with_capacity(sorted.len());

    for kid in sorted {
        if *budget == 0 {
            break;
        }
        *budget -= 1;

        let e = &analysis.snapshot.entries[kid as usize];
        let path = analysis.snapshot.abs_path(kid);
        // 子项的判定：走权威归因（与 explain / 执行校验同一份逻辑）。
        // 不能沿用父项的判定 —— 用户往下看正是为了发现"这里不一样"。
        let attr = analysis
            .rules
            .attribute_path(&path, e.is_dir)
            .map(|i| analysis.rules.rule(i));

        let (
            safety,
            safety_label,
            rule_id,
            rule_name,
            category_label,
            kind,
            cmd,
            can_pick,
        ) = match attr {
            Some(r) => {
                let (k, c) = action_of(&r.action);
                (
                    r.safety,
                    r.safety.label().to_string(),
                    r.id.clone(),
                    r.name.clone(),
                    r.category.label().to_string(),
                    k.to_string(),
                    c,
                    selectable_for(r.safety, &r.action),
                )
            }
            // 未识别：如实说"不知道"，不借用父项的判定
            None => (
                Safety::Caution,
                "未识别".to_string(),
                String::new(),
                "规则库未覆盖".to_string(),
                "未知".to_string(),
                "review".to_string(),
                None,
                false,
            ),
        };

        let children = if e.size >= threshold && e.is_dir {
            expand_subtree(
                analysis,
                index,
                &path,
                safety,
                threshold,
                limit,
                depth + 1,
                budget,
                // 子项是目录时，分母用它在快照里的真实大小
                analysis
                    .snapshot
                    .entries
                    .get(kid as usize)
                    .map(|e| e.size)
                    .unwrap_or(e.size),
            )
        } else {
            Vec::new()
        };

        out.push(SubItem {
            name: e.name.to_string(),
            path,
            size: e.size,
            share: round1(e.size as f64 / parent_size as f64 * 100.0),
            is_dir: e.is_dir,
            file_count: index.files_of.get(&kid).copied().unwrap_or(0),
            safety: safety.key().to_string(),
            safety_label,
            rule_id,
            rule_name,
            category_label,
            action_kind: kind,
            action_command: cmd,
            selectable: can_pick,
            differs_from_parent: safety != parent_safety,
            expandable: !children.is_empty(),
            children,
        });
    }

    // 子项多于 limit 时，把剩余的并成一个"其他 N 项"条目 ——
    // 直接丢掉会让各子项之和小于父项，用户会以为数字不对。
    if total_kids > limit {
        let shown: u64 = out.iter().map(|s| s.size).sum();
        let rest = parent_size.saturating_sub(shown);
        if rest > 0 {
            out.push(SubItem {
                name: format!("其他 {} 项", total_kids - limit),
                path: String::new(),
                size: rest,
                share: round1(rest as f64 / parent_size as f64 * 100.0),
                is_dir: false,
                file_count: 0,
                safety: parent_safety.key().to_string(),
                safety_label: "（聚合显示）".to_string(),
                rule_id: String::new(),
                rule_name: String::new(),
                category_label: String::new(),
                action_kind: "none".to_string(),
                action_command: None,
                selectable: false,
                differs_from_parent: false,
                expandable: false,
                children: Vec::new(),
            });
        }
    }

    out
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
/// 判断一个规则判定结果能不能在界面上勾选（即"移入暂存区"）。
///
/// 两个条件必须同时满足：
///
/// 1. **安全等级**是 `safe` 或 `caution` —— 它们要么可重建、要么需要人判断，
///    而判断权本来就该在用户手里，工具的责任是说清风险。
///    `system-managed` 走官方命令（直接移走会破坏系统）、`protected` 是硬黑名单、
///    `keep` 是用户数据（本工具不碰）—— 给它们勾选框等于给了个点了会出错的假按钮。
///
/// 2. **处置方式是 `Delete`**。这一条容易漏：像 pip / npm 这类缓存，
///    规则明确写了"执行官方命令"，因为官方命令除了清文件还会维护索引状态
///    （例如 `pip cache purge` 会同步更新缓存元数据）。单纯把目录移走虽然
///    也能腾出空间，但可能让工具的状态与实际不一致。
///    **既然规则作者写了该用命令，界面就不该提供另一种做法。**
fn selectable_for(safety: Safety, action: &Action) -> bool {
    matches!(safety, Safety::Safe | Safety::Caution) && matches!(action, Action::Delete)
}

fn is_selectable(f: &dd_analyze::Finding) -> bool {
    selectable_for(f.safety, &f.action)
}

fn build_summary(items: &[SurveyItem]) -> Vec<SummaryRow> {
    let order = [
        Safety::Safe,
        Safety::Caution,
        Safety::Keep,
        Safety::SystemManaged,
        Safety::Protected,
    ];
    let mut out = Vec::new();
    for s in order {
        let key = s.key().to_string();
        let sel: Vec<&SurveyItem> = items
            .iter()
            .filter(|i| i.safety == key)
            .collect();
        if sel.is_empty() {
            continue;
        }
        out.push(SummaryRow {
            safety: key,
            label: s.label().to_string(),
            count: sel.len(),
            selectable_count: sel.iter().filter(|i| i.selectable).count(),
            bytes: sel.iter().map(|i| i.size).sum(),
        });
    }
    out
}

fn now_text() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, dd) = crate::civil_from_days_pub(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        dd,
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// 从路径生成一个安全的文件名片段（`C:\Users\me` → `C-Users-me`）。
fn safe_stem(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "disk".into()
    } else if out.chars().count() > 40 {
        out.chars().take(40).collect()
    } else {
        out
    }
}

fn open_in_browser(p: &Path) {
    #[cfg(windows)]
    {
        // 用 cmd /c start 而不是直接调用浏览器：后者要处理默认浏览器的路径解析，
        // 交给系统 shell 最省事。
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", &p.to_string_lossy()])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(p).spawn();
    }
}

// ------------------------------------------------------------------ HTML 模板

const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>磁盘内容清单 · DiskDoctor</title>
<style>
  /* ------------------------------------------------------------ 设计令牌
     底色分三层：页面 / 面板 / 抬升。深色界面里靠**色阶**区分层级比靠边框
     更干净，也让"卡片浮在上面"这件事一眼可见。

     描边统一用带透明度的白，而不是实色灰 —— 实色描边会和面板色互相干扰，
     换底色时还得跟着调；半透明白在任何底色上都成立。 */
  :root {
    --bg:      #0a0d14;
    --panel:   #111725;
    --panel-2: #161e2e;
    --panel-3: #1d2739;

    --border:      rgba(255,255,255,.09);
    --border-soft: rgba(255,255,255,.055);
    --border-hi:   rgba(255,255,255,.17);

    --fg:       #e9eff8;
    --fg-dim:   #9aa8bc;
    /* 次要文字。曾经用 #6b7a8f，实测在面板上只有 4.10:1，达不到 WCAG AA 的
       4.5:1 —— 而它承载的是标签、百分比、提示这些**需要看清**的信息，
       不是装饰。调亮到 #8794aa 后有 5.56:1。 */
    --fg-faint: #8794aa;

    --safe:      #48d17c;
    --caution:   #e5b544;
    --keep:      #5aa9f8;
    --system:    #8d99a9;
    --protected: #f2685f;
    --accent:    #4a90f7;

    --r-sm: 6px;
    --r-md: 10px;
    --r-lg: 14px;

    --shadow-1: 0 1px 2px rgba(0,0,0,.35);
    --shadow-2: 0 6px 20px rgba(0,0,0,.38);
    --shadow-3: 0 16px 44px rgba(0,0,0,.5);

    --mono: ui-monospace, SFMono-Regular, "Cascadia Mono", Consolas, monospace;
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; }
  body {
    background: var(--bg);
    color: var(--fg);
    font: 14px/1.65 "Segoe UI", -apple-system, BlinkMacSystemFont,
          "Microsoft YaHei", system-ui, sans-serif;
    padding-bottom: 104px;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
  }
  /* 数字一律等宽对齐。这一页到处都是可比数值，比例字体下「1」比「8」窄，
     竖排的金额和百分比会参差不齐。 */
  .card .v, .row .size .b, .kid .pc, .kid .sz,
  .dock .stat b, .comp .lg b, .bar-none {
    font-variant-numeric: tabular-nums;
    font-feature-settings: "tnum" 1;
  }
  .wrap { max-width: 1240px; margin: 0 auto; padding: 32px 26px 0; }

  header { margin-bottom: 22px; }
  header h1 {
    font-size: 21px; margin: 0 0 7px; font-weight: 650;
    letter-spacing: -.2px;
  }
  .meta { color: var(--fg-dim); font-size: 12.5px; line-height: 1.75; }
  .meta code {
    color: var(--fg); background: var(--panel-2);
    border: 1px solid var(--border-soft);
    padding: 1.5px 7px; border-radius: var(--r-sm);
    font-family: var(--mono); font-size: 12px;
  }

  /* ------------------------------------------------------------ 汇总卡片
     每张卡片的重点只有一个：那个数字。所以数字做大、标签做小。
     底下再加一条"占全盘多少"的细条 —— 「禁止触碰 39 GB」和「可安全回收
     1.4 GB」写在一行里看不出差别，画成长度就一眼分明。 */
  .cards {
    display: grid; gap: 10px; margin: 0 0 14px;
    grid-template-columns: repeat(auto-fit, minmax(174px, 1fr));
  }
  .card {
    position: relative; overflow: hidden;
    background: linear-gradient(180deg, var(--panel-2) 0%, var(--panel) 100%);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    padding: 14px 15px 13px;
    box-shadow: var(--shadow-1);
  }
  /* 顶部一道细色带，用语义色标示这张卡属于哪一档 */
  .card::before {
    content: ""; position: absolute; left: 0; right: 0; top: 0; height: 2px;
    background: var(--card-accent, var(--border-hi));
    opacity: .9;
  }
  .card .k {
    font-size: 11.5px; color: var(--fg-dim); font-weight: 500;
    display: flex; align-items: center; gap: 6px;
  }
  .card .v {
    font-size: 22px; font-weight: 650; margin-top: 5px;
    letter-spacing: -.4px; line-height: 1.2;
  }
  .card .n { font-size: 11.5px; color: var(--fg-faint); margin-top: 3px; }
  /* 该档占全盘的比重 */
  .card .share {
    height: 3px; border-radius: 2px; background: var(--border-soft);
    margin-top: 10px; overflow: hidden;
  }
  .card .share > i {
    display: block; height: 100%; border-radius: 2px;
    background: var(--card-accent, var(--accent));
    min-width: 2px;
  }
  .dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; flex: none; }
  .dot.safe { background: var(--safe); } .dot.caution { background: var(--caution); }
  .dot.keep { background: var(--keep); } .dot.system-managed { background: var(--system); }
  .dot.protected { background: var(--protected); }

  /* --------------------------------------------------------------- 构成条
     一条横向堆叠条 + 图例。放在最前面，因为它回答的是那个最要紧的问题：
     「这块盘里，真正能安全清掉的到底有多少」。 */
  .comp { margin: 0 0 18px; }  .comp .bar {
    display: flex; height: 12px; border-radius: 999px; overflow: hidden;
    background: var(--panel-2); border: 1px solid var(--border);
  }
  .comp .bar > i {
    display: block; height: 100%; min-width: 3px;
    transition: filter .15s;
  }
  .comp .bar > i:hover { filter: brightness(1.3); }
  .comp .lg {
    display: flex; gap: 5px 18px; flex-wrap: wrap;
    margin-top: 11px; font-size: 12px; color: var(--fg-dim);
  }
  .comp .lg span { display: inline-flex; align-items: center; gap: 6px; }
  .comp .lg b { color: var(--fg); font-weight: 600; }
  .comp .lg em { color: var(--fg-faint); font-style: normal; font-size: 11.5px; }

  /* 提示框：用一道从语义色渐变到透明的底纹暗示方向，
     比纯色块轻，又比只有左边框更能看出"这是一条提示"。 */
  .note {
    background: linear-gradient(90deg, rgba(74,144,247,.075), transparent 62%), var(--panel);
    border: 1px solid var(--border-soft);
    border-left: 3px solid var(--accent);
    border-radius: var(--r-md);
    padding: 12px 15px; margin: 0 0 12px;
    font-size: 12.5px; line-height: 1.75; color: var(--fg-dim);
  }
  .note b { color: var(--fg); font-weight: 600; }
  .note.warn {
    background: linear-gradient(90deg, rgba(229,181,68,.08), transparent 62%), var(--panel);
    border-left-color: var(--caution);
  }
  .note code {
    font-family: var(--mono); font-size: 11.5px;
    background: var(--panel-3); border: 1px solid var(--border-soft);
    border-radius: 4px; padding: 1px 5px; color: var(--fg);
  }

  /* ------------------------------------------------------------- 工具栏 */
  .toolbar {
    position: sticky; top: 0; z-index: 20;
    background: rgba(10,13,20,.88);
    backdrop-filter: blur(14px) saturate(1.4);
    -webkit-backdrop-filter: blur(14px) saturate(1.4);
    border-bottom: 1px solid var(--border);
    padding: 13px 0 12px; margin-bottom: 16px;
    display: flex; gap: 10px; flex-wrap: wrap; align-items: center;
  }
  input[type=search], select {
    background: var(--panel); color: var(--fg);
    border: 1px solid var(--border); border-radius: var(--r-sm);
    padding: 7px 11px; font-size: 13px; font-family: inherit;
    transition: border-color .15s, box-shadow .15s;
  }
  input[type=search] { flex: 1 1 240px; min-width: 180px; }
  input[type=search]::placeholder { color: var(--fg-faint); }
  select { cursor: pointer; }
  input[type=search]:focus, select:focus {
    outline: none; border-color: var(--accent);
    box-shadow: 0 0 0 3px rgba(74,144,247,.16);
  }
  .chips { display: flex; gap: 6px; flex-wrap: wrap; }
  .chip {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 999px; padding: 5px 12px; font-size: 12px;
    cursor: pointer; user-select: none; color: var(--fg-faint);
    display: flex; align-items: center; gap: 6px;
    transition: color .15s, border-color .15s, background .15s;
  }
  .chip:hover { color: var(--fg-dim); border-color: var(--border-hi); }
  .chip.on {
    color: var(--fg); border-color: var(--border-hi);
    background: var(--panel-3);
  }
  /* 未勾选的筛选片压暗，让"当前在看哪几档"一眼可辨 */
  .chip:not(.on) .dot { opacity: .35; }
  .chip input { margin: 0; accent-color: var(--accent); }

  /* --------------------------------------------------------------- 列表行
     一条行里最该被扫到的是**右边的体积** —— 用户是来找"谁在占地方"的。
     所以路径走等宽小字、体积走大字号，层级拉开。 */
  .row {
    position: relative;
    background: var(--panel); border: 1px solid var(--border-soft);
    border-radius: var(--r-md); padding: 13px 15px; margin-bottom: 7px;
    display: flex; gap: 13px; align-items: flex-start;
    transition: border-color .15s, background .15s, box-shadow .15s;
  }
  .row:hover {
    border-color: var(--border-hi);
    box-shadow: var(--shadow-2);
  }
  /* 选中态：左边一道竖条，比整块变色克制，但一眼能扫到选了哪些 */
  .row.picked {
    border-color: rgba(74,144,247,.55);
    background: var(--panel-2);
  }
  .row.picked::before {
    content: ""; position: absolute; left: 0; top: 10px; bottom: 10px;
    width: 3px; border-radius: 0 3px 3px 0; background: var(--accent);
  }
  /* 锁定的项压暗但不隐藏 —— 用户仍要看得到"这里有东西，只是不归我管" */
  .row.locked { opacity: .58; }
  .row.locked:hover { opacity: .78; border-color: var(--border); }
  .row .cb { padding-top: 3px; }
  .row .cb input {
    width: 16px; height: 16px; cursor: pointer; margin: 0;
    accent-color: var(--accent);
  }
  .row .cb input:disabled { cursor: not-allowed; }
  .row .main { flex: 1; min-width: 0; }
  .row .p {
    font-family: var(--mono);
    font-size: 12.5px; word-break: break-all; line-height: 1.5;
    letter-spacing: -.1px;
  }

  /* ---- 可点击的路径 ---- */
  .pth {
    cursor: pointer; border-radius: 3px;
    border-bottom: 1px dashed var(--border-hi);
    transition: color .12s, border-bottom-color .12s;
  }
  .pth:hover { color: var(--accent); border-bottom-color: var(--accent); }
  .pth:active { color: var(--accent); opacity: .7; }
  .pth::after {
    content: "↗"; font-size: 10px; opacity: .4; margin-left: 5px;
    vertical-align: super;
  }
  .pth:hover::after { opacity: 1; }

  /* ---- 轻提示（点击打开之后给个反馈）---- */
  .toast {
    position: fixed; left: 50%; bottom: 88px; z-index: 80;
    max-width: min(760px, 92vw);
    transform: translateX(-50%) translateY(10px);
    background: var(--panel-3); border: 1px solid rgba(74,144,247,.5);
    border-radius: var(--r-md); padding: 10px 15px;
    font-family: var(--mono);
    font-size: 12px; line-height: 1.5; color: var(--fg);
    word-break: break-all;
    box-shadow: var(--shadow-3);
    opacity: 0; pointer-events: none;
    transition: opacity .18s, transform .18s;
  }
  .toast.show { opacity: 1; transform: translateX(-50%) translateY(0); }

  /* ---- 路径浮层 ---- */
  .pathpop {
    position: fixed; z-index: 60; max-width: min(640px, 92vw);
    background: var(--panel-3); border: 1px solid var(--border-hi);
    border-radius: var(--r-lg); padding: 14px 15px;
    box-shadow: var(--shadow-3);
  }
  .pathpop .pp-path {
    font-family: var(--mono);
    font-size: 12.5px; line-height: 1.55; word-break: break-all;
    color: var(--fg); user-select: text;
  }
  .pathpop .pp-kind {
    font-size: 11.5px; color: var(--fg-faint); margin-top: 6px;
  }
  .pathpop .pp-act {
    display: flex; gap: 8px; flex-wrap: wrap; margin-top: 12px;
    align-items: center;
  }
  .pathpop .pp-hint {
    font-size: 11.5px; color: var(--fg-faint); margin-top: 10px;
    line-height: 1.6; border-top: 1px solid var(--border-soft);
    padding-top: 9px;
  }
  .pathpop .pp-hint code {
    background: var(--panel-2); border: 1px solid var(--border);
    border-radius: 4px; padding: 1px 6px;
    font-family: var(--mono); font-size: 11px;
    user-select: all; color: var(--fg);
  }
  .pathpop button.primary { border-color: rgba(74,144,247,.6); color: var(--accent); }

  /* ------------------------------------------------------------ 标签 / 徽章 */
  .row .tags {
    margin: 8px 0 0; display: flex; gap: 7px; flex-wrap: wrap;
    align-items: center;
  }
  .badge {
    font-size: 11.5px; padding: 2.5px 9px; border-radius: 999px;
    border: 1px solid; display: inline-flex; align-items: center; gap: 5px;
    font-weight: 500;
  }
  .badge.safe { color: var(--safe); border-color: rgba(72,209,124,.38); background: rgba(72,209,124,.1); }
  .badge.caution { color: var(--caution); border-color: rgba(229,181,68,.38); background: rgba(229,181,68,.1); }
  .badge.keep { color: var(--keep); border-color: rgba(90,169,248,.38); background: rgba(90,169,248,.1); }
  .badge.system-managed { color: var(--system); border-color: rgba(141,153,169,.38); background: rgba(141,153,169,.1); }
  .badge.protected { color: var(--protected); border-color: rgba(242,104,95,.38); background: rgba(242,104,95,.1); }
  .tag { font-size: 11.5px; color: var(--fg-faint); }
  .why {
    margin: 9px 0 0; font-size: 12.5px; color: var(--fg-dim);
    border-left: 2px solid var(--border); padding-left: 10px;
    line-height: 1.7;
  }
  .rec {
    font-size: 12px; color: var(--fg-faint); margin-top: 5px;
    padding-left: 12px; line-height: 1.7;
  }
  .row .size {
    text-align: right; flex: none; min-width: 104px;
  }
  .row .size .b { font-size: 16px; font-weight: 650; letter-spacing: -.3px; }
  .row .size .s { font-size: 11.5px; color: var(--fg-faint); margin-top: 1px; }

  /* ---------------------------------------------------------------- 底栏 */
  .dock {
    position: fixed; left: 0; right: 0; bottom: 0; z-index: 30;
    background: rgba(17,23,37,.94);
    backdrop-filter: blur(14px) saturate(1.4);
    -webkit-backdrop-filter: blur(14px) saturate(1.4);
    border-top: 1px solid var(--border);
    box-shadow: 0 -6px 24px rgba(0,0,0,.4);
    padding: 13px 26px;
  }
  .dock .in {
    max-width: 1240px; margin: 0 auto;
    display: flex; gap: 12px; align-items: center; flex-wrap: wrap;
  }
  .dock .stat { font-size: 13px; color: var(--fg-dim); }
  .dock .stat b { color: var(--fg); font-size: 15px; font-weight: 650; }
  /* 有选择时用青色提示"可以往下走了" */
  .dock.active .stat b { color: var(--safe); }
  .dock .spacer { flex: 1; }
  button {
    font-family: inherit; font-size: 13px; border-radius: var(--r-sm);
    padding: 8px 15px; cursor: pointer; border: 1px solid var(--border);
    background: var(--panel-3); color: var(--fg);
    transition: border-color .15s, background .15s, box-shadow .15s, transform .08s;
  }
  button:hover:not(:disabled) {
    border-color: var(--border-hi); background: var(--panel-2);
  }
  button:active:not(:disabled) { transform: translateY(1px); }
  button:focus-visible {
    outline: none; box-shadow: 0 0 0 3px rgba(74,144,247,.28);
  }
  button.primary {
    background: var(--accent); border-color: var(--accent);
    /* 用深色字而不是白字：白色在 #4a90f7 上只有 3.17:1，达不到 AA；
       换成近黑的深蓝有 5.93:1。顺带也更醒目 —— 亮底深字比亮底白字
       的对比更强。 */
    color: #071320; font-weight: 650;
    box-shadow: 0 2px 10px rgba(74,144,247,.3);
  }
  button.primary:hover:not(:disabled) {
    background: #63a1f9; border-color: #63a1f9;
    box-shadow: 0 3px 14px rgba(74,144,247,.45);
  }
  button:disabled { opacity: .4; cursor: not-allowed; box-shadow: none; }
  .hint { font-size: 12px; color: var(--fg-faint); }

  .empty { text-align: center; color: var(--fg-faint); padding: 56px 0; font-size: 13.5px; }

  /* ------------------------------------------------- 细分（大条目展开看内部）
     左侧一道竖线表示"这些是上一层的内容"，而不是重新起一层卡片 ——
     嵌套卡片在深色界面里会糊成一片。 */
  .kids {
    margin: 12px 0 2px; border-left: 2px solid var(--border);
    padding-left: 14px;
  }
  .kids-hd {
    display: flex; align-items: center; gap: 8px; cursor: pointer;
    font-size: 12.5px; color: var(--fg-dim); user-select: none;
    padding: 4px 0; list-style: none;
  }
  .kids-hd::-webkit-details-marker { display: none; }
  .kids-hd:hover { color: var(--fg); }
  .kids-hd .arrow { transition: transform .15s; display: inline-block; font-size: 9px; }
  .kids-hd.open .arrow { transform: rotate(90deg); }
  .kid {
    display: flex; gap: 10px; align-items: center;
    padding: 6px 8px; border-radius: var(--r-sm);
    font-size: 12.5px;
    transition: background .12s;
  }
  .kid:hover { background: rgba(255,255,255,.028); }
  .kid .nm { flex: 1; min-width: 0; word-break: break-all; }
  .kid .nm .sub { color: var(--fg-faint); font-size: 11.5px; margin-left: 7px; }
  .kid .pc {
    flex: none; width: 50px; text-align: right; color: var(--fg-faint);
    font-size: 11.5px;
  }
  .kid .sz { flex: none; width: 82px; text-align: right; color: var(--fg-dim); }
  .kid .act { flex: none; width: 84px; text-align: right; }
  .kid .mini {
    font-size: 11px; padding: 2px 7px; border-radius: 999px;
    border: 1px solid; white-space: nowrap;
  }
  .kid .mini.safe { color: var(--safe); border-color: rgba(72,209,124,.35); }
  .kid .mini.caution { color: var(--caution); border-color: rgba(229,181,68,.35); }
  .kid .mini.keep { color: var(--keep); border-color: rgba(90,169,248,.35); }
  .kid .mini.system-managed { color: var(--system); border-color: rgba(141,153,169,.35); }
  .kid .mini.protected { color: var(--protected); border-color: rgba(242,104,95,.35); }
  /* 判定与父项不同 —— 这是"这里藏着一个不一样的东西"的信号 */
  .kid.diff { background: rgba(229,181,68,.07); }
  .kid.diff:hover { background: rgba(229,181,68,.11); }
  .kid.diff .nm::before { content: "▲ "; color: var(--caution); font-size: 9px; }

  /* 占比条。颜色跟着安全等级走 —— 这样"哪块能清、哪块不能"
     不用读文字就能看出来。 */
  .kid .bar {
    flex: none; width: 80px; height: 6px; border-radius: 3px;
    background: rgba(255,255,255,.07); overflow: hidden;
  }
  .kid .bar > i {
    display: block; height: 100%; border-radius: 3px; min-width: 2px;
    background: var(--accent);
  }
  .kid .bar > i.safe { background: var(--safe); }
  .kid .bar > i.caution { background: var(--caution); }
  .kid .bar > i.keep { background: var(--keep); }
  .kid .bar > i.system-managed { background: var(--system); }
  .kid .bar > i.protected { background: var(--protected); }
  .kid.aggr { color: var(--fg-faint); font-style: italic; }
  .kid input[type=checkbox] {
    width: 14px; height: 14px; margin: 0; cursor: pointer;
    accent-color: var(--accent);
  }
  .kids-note { font-size: 11.5px; color: var(--fg-faint); padding: 2px 0; }
  .cmd {
    margin-top: 9px; display: flex; gap: 8px; align-items: center; flex-wrap: wrap;
  }
  .cmd code {
    background: var(--panel-3); border: 1px solid var(--border-soft);
    border-radius: 5px; padding: 4px 9px;
    font-family: var(--mono); font-size: 12px; color: var(--fg);
  }
  button.copy { padding: 4px 11px; font-size: 12px; }
  kbd {
    background: var(--panel-3); border: 1px solid var(--border);
    border-bottom-width: 2px; border-radius: 4px; padding: 0 6px;
    font-size: 11.5px; font-family: var(--mono); color: var(--fg-dim);
  }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <h1>磁盘内容清单</h1>
    <div class="meta" id="meta"></div>
  </header>

  <div class="cards" id="cards"></div>
  <div class="comp" id="comp"></div>

  <div id="notes"></div>

  <div class="toolbar">
    <input type="search" id="q" placeholder="搜索路径…（例如 node_modules、Temp）">
    <div class="chips" id="chips"></div>
    <select id="cat"><option value="">全部类别</option></select>
    <select id="sort">
      <option value="size">按体积</option>
      <option value="name">按路径</option>
      <option value="safety">按安全等级</option>
    </select>
  </div>

  <main id="list"></main>
</div>

<div class="dock">
  <div class="in">
    <div class="stat" id="dock-stat">尚未选择</div>
    <div class="spacer"></div>
    <span class="hint" id="dock-hint"></span>
    <button id="pick-safe">选中全部「可安全回收」</button>
    <button id="clear">清空选择</button>
    <button class="primary" id="export" disabled>导出清单</button>
  </div>
</div>

<script>
const SERVER = __SERVER__;
const DATA = __DATA__;
const SAFETY_ORDER = ["safe","caution","keep","system-managed","protected"];

function human(n) {
  if (n >= 1099511627776) return (n/1099511627776).toFixed(2) + " TB";
  if (n >= 1073741824) return (n/1073741824).toFixed(2) + " GB";
  if (n >= 1048576) return (n/1048576).toFixed(1) + " MB";
  if (n >= 1024) return (n/1024).toFixed(1) + " KB";
  return n + " B";
}
function timeText(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  const p = n => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth()+1)}-${p(d.getDate())}`;
}
function esc(s) {
  return String(s).replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#39;"}[c]));
}

// ---------- 顶部信息 ----------
document.getElementById("meta").innerHTML =
  `扫描 <code>${esc(DATA.root)}</code> ｜ ${DATA.file_count.toLocaleString()} 个文件 / ` +
  `${DATA.dir_count.toLocaleString()} 个目录 ｜ 生成于 ${esc(DATA.scanned_at)}`;

// ---------- 汇总卡片 ----------
//
// 每张卡片把一个数字做大，再加一条"占全盘多少"的细条。
// 为什么要有那条：39 GB 的「禁止触碰」和 1.4 GB 的「可安全回收」都写成
// 一行数字时看不出差别，画成长度就一眼分明 —— 这也直接回答了
// 「这块盘里真正能清的有多少」。
const SAFETY_VAR = {
  "safe": "var(--safe)", "caution": "var(--caution)", "keep": "var(--keep)",
  "system-managed": "var(--system)", "protected": "var(--protected)",
};
const totalBytes = Math.max(1, DATA.summary.reduce((a, s) => a + s.bytes, 0));

const cards = document.getElementById("cards");
DATA.summary.forEach(s => {
  const pct = s.bytes / totalBytes * 100;
  const el = document.createElement("div");
  el.className = "card";
  el.style.setProperty("--card-accent", SAFETY_VAR[s.safety] || "var(--border-hi)");
  el.innerHTML =
    `<div class="k"><span class="dot ${s.safety}"></span>${esc(s.label)}</div>` +
    `<div class="v">${human(s.bytes)}</div>` +
    `<div class="n">${s.count} 处` +
      (s.selectable_count > 0 ? ` · 可勾 ${s.selectable_count}` : " · 不可勾选") +
    `</div>` +
    `<div class="share" title="占已归因内容的 ${pct.toFixed(1)}%">` +
      `<i style="width:${Math.max(pct, 0.7).toFixed(2)}%"></i></div>`;
  cards.appendChild(el);
});

// ---------- 构成条 ----------
//
// 一条横向堆叠条，放在最前面。它回答的是整页最要紧的问题：
// 「这堆东西里，真正能安全清掉的到底占多少」。
// 只有一档（或没有数据）时不画 —— 一条纯色长条不提供任何信息。
const comp = document.getElementById("comp");
const shownTiers = DATA.summary.filter(s => s.bytes > 0);
if (shownTiers.length > 1) {
  const segs = shownTiers.map(s => {
    const pct = s.bytes / totalBytes * 100;
    return `<i class="${s.safety}" style="width:${pct.toFixed(3)}%;` +
      `background:${SAFETY_VAR[s.safety] || "var(--accent)"}" ` +
      `title="${esc(s.label)} ${human(s.bytes)}（${pct.toFixed(1)}%）"></i>`;
  }).join("");
  const lg = shownTiers.map(s => {
    const pct = s.bytes / totalBytes * 100;
    return `<span><span class="dot ${s.safety}"></span>${esc(s.label)} ` +
      `<b>${human(s.bytes)}</b> <em>${pct.toFixed(1)}%</em></span>`;
  }).join("");
  comp.innerHTML = `<div class="bar">${segs}</div><div class="lg">${lg}</div>`;
}

// 回收站等系统级项目，单独提示
const notes = document.getElementById("notes");
if (DATA.folded_count > 0) {
  const d = document.createElement("div");
  d.className = "note";
  d.innerHTML = `另有 <b>${DATA.folded_count}</b> 个较小的条目（合计 <b>${human(DATA.folded_bytes)}</b>）` +
    `未逐条列出 —— 它们大多是几十字节的缓存，加起来也省不下多少，逐条列出只会淹没重点。` +
    `想看全部请用 <code>--merge-below-kb 0</code> 重新生成。`;
  notes.appendChild(d);
}
if (DATA.unattributed > 0) {
  const d = document.createElement("div");
  d.className = "note warn";
  d.innerHTML = `有 <b>${human(DATA.unattributed)}</b> 的内容规则库尚未覆盖（未识别）。` +
    `这部分工具<b>无法告诉你能不能删</b> —— 它属于"不知道"，不是"安全"。需要你自己判断，或补充规则。`;
  notes.appendChild(d);
}

// ---------- 安全等级筛选 ----------
const chips = document.getElementById("chips");
DATA.summary.forEach(s => {
  const lab = document.createElement("label");
  lab.className = "chip on";
  lab.dataset.safety = s.safety;
  lab.innerHTML = `<input type="checkbox" checked data-k="${s.safety}">` +
    `<span class="dot ${s.safety}"></span>${esc(s.label)}`;
  lab.querySelector("input").addEventListener("change", e => {
    lab.classList.toggle("on", e.target.checked);
    render();
  });
  chips.appendChild(lab);
});

// ---------- 类别下拉 ----------
const catSel = document.getElementById("cat");
[...new Set(DATA.items.map(i => i.category))].sort().forEach(c => {
  const label = (DATA.items.find(i => i.category === c) || {}).category_label || c;
  const o = document.createElement("option");
  o.value = c; o.textContent = label;
  catSel.appendChild(o);
});

// ---------- 列表渲染 ----------
const list = document.getElementById("list");
const picked = new Set();
let visible = [];

function currentFilters() {
  const q = document.getElementById("q").value.trim().toLowerCase();
  const on = new Set([...chips.querySelectorAll("input:checked")].map(i => i.dataset.k));
  const cat = catSel.value;
  const sort = document.getElementById("sort").value;
  return { q, on, cat, sort };
}

function render() {
  const { q, on, cat, sort } = currentFilters();
  visible = DATA.items.filter(i =>
    on.has(i.safety) &&
    (!cat || i.category === cat) &&
    (!q || i.path.toLowerCase().includes(q))
  );

  visible.sort((a, b) => {
    if (sort === "name") return a.path.localeCompare(b.path);
    if (sort === "safety") {
      const d = SAFETY_ORDER.indexOf(a.safety) - SAFETY_ORDER.indexOf(b.safety);
      return d !== 0 ? d : b.size - a.size;
    }
    return b.size - a.size;
  });

  if (visible.length === 0) {
    list.innerHTML = `<div class="empty">没有匹配的条目</div>`;
  } else {
    const frag = document.createDocumentFragment();
    visible.forEach(it => frag.appendChild(rowEl(it)));
    list.replaceChildren(frag);
  }
  updateDock();
}

function rowEl(it) {
  const el = document.createElement("div");
  el.className = "row" + (it.selectable ? "" : " locked") + (picked.has(it.path) ? " picked" : "");

  const bits = [];
  bits.push(`<span class="badge ${it.safety}"><span class="dot ${it.safety}"></span>${esc(it.safety_label)}</span>`);
  bits.push(`<span class="tag">${esc(it.category_label)}</span>`);
  if (it.is_dir && it.file_count > 0) bits.push(`<span class="tag">含 ${it.file_count.toLocaleString()} 个文件</span>`);
  if (it.mtime) bits.push(`<span class="tag">最后修改 ${timeText(it.mtime)}</span>`);
  if (it.inherited) bits.push(`<span class="tag">判定继承自上级目录</span>`);

  // 呈现方式按处置类型分三种，因为「正确的做法」本来就不同：
  //   delete  → 给勾选框，用户可以挑（移入暂存区）
  //   command → 给一条可复制的官方命令（工具不该代跑，也不该绕过）
  //   其他    → 只说明该怎么处理
  let lockNote = "";
  if (it.action_kind === "command" && it.action_command) {
    lockNote = `<div class="cmd"><code>${esc(it.action_command)}</code>` +
      `<button class="copy" data-cmd="${esc(it.action_command)}">复制命令</button></div>`;
  } else if (!it.selectable) {
    lockNote = `<div class="rec">${esc(it.action)}</div>`;
  }

  el.innerHTML =
    `<div class="cb"><input type="checkbox" ${it.selectable ? "" : "disabled"} ` +
      `${picked.has(it.path) ? "checked" : ""}></div>` +
    `<div class="main">` +
      // title 里换行要用 &#10; 实体（直接写 \n 不生效）
    `<div class="p"><span class="pth" data-path="${esc(it.path)}" ` +
        `data-dir="${it.is_dir ? "1" : "0"}" ` +
        `title="点击打开：${esc(it.path)}&#10;右键或 Ctrl+点击 = 复制路径">` +
        `${esc(it.path)}</span></div>` +
      `<div class="tags">${bits.join("")}</div>` +
      (it.why ? `<div class="why">${esc(it.why)}</div>` : "") +
      (it.recovery ? `<div class="rec">恢复：${esc(it.recovery)}</div>` : "") +
      lockNote +
    `</div>` +
    `<div class="size"><div class="b">${human(it.size)}</div>` +
      `${it.is_dir ? `<div class="s">目录</div>` : `<div class="s">文件</div>`}</div>`;

  // 大条目的细分（"里面装了什么"）—— 默认折叠，点开才展开
  if (it.children && it.children.length) {
    const det = document.createElement("details");
    det.className = "kids";
    const sum = document.createElement("summary");
    sum.className = "kids-hd";
    // 目录真实总大小与"归因区域大小"可能不同：区域只统计归本规则管的部分。
    // 两个数字都给出并解释，否则用户会以为工具算错了。
    let hd = `<span class="arrow">▶</span>展开看内部构成（${it.children.length} 项）`;
    if (it.dir_total && it.dir_total > it.size) {
      hd += `<span class="kids-note"> ｜ 该目录实际共 ${human(it.dir_total)}，` +
            `其中 ${human(it.dir_total - it.size)} 归属其他规则（已单独列出）</span>`;
    }
    sum.innerHTML = hd;
    det.appendChild(sum);

    const box = document.createElement("div");
    renderKids(it.children, box, 0);
    det.appendChild(box);

    det.addEventListener("toggle", () => sum.classList.toggle("open", det.open));
    el.querySelector(".main").appendChild(det);
  }

  const cb = el.querySelector("input");
  if (cb) {
    cb.addEventListener("change", e => {
      if (e.target.checked) picked.add(it.path); else picked.delete(it.path);
      el.classList.toggle("picked", e.target.checked);
      updateDock();
    });
  }
  return el;
}

// 递归渲染子项。子项**也可以勾选** —— 这正是细分的价值：
// 看到 AppData\Local 里面某块 6 GB 且可回收，就能直接勾它。
function renderKids(kids, host, depth) {
  kids.forEach(k => {
    const row = document.createElement("div");
    row.className = "kid" + (k.differs_from_parent ? " diff" : "")
      + (k.path ? "" : " aggr");

    const bits = [];
    if (k.rule_name) bits.push(esc(k.rule_name));
    if (k.is_dir && k.file_count) bits.push(k.file_count.toLocaleString() + " 个文件");
    if (k.differs_from_parent) bits.push("判定与上层不同");

    let act = "";
    if (k.selectable) {
      act = `<input type="checkbox" data-path="${esc(k.path)}">`;
    } else if (k.action_kind === "command" && k.action_command) {
      act = `<button class="copy mini" data-cmd="${esc(k.action_command)}">复制命令</button>`;
    } else {
      act = `<span class="tag">${esc(k.safety_label)}</span>`;
    }

    // 子项这里显示的是短名，但 `k.path` 是完整路径 —— 点一下正好
    // 直接跳过去（用户常常只看到目录名，不知道它在哪一层）。
    const nmHtml = k.path
      ? `<span class="pth" data-path="${esc(k.path)}" data-dir="${k.is_dir ? "1" : "0"}" ` +
        `title="点击打开：${esc(k.path)}&#10;右键或 Ctrl+点击 = 复制路径">` +
        `${esc(k.name)}</span>`
      : esc(k.name);

    row.innerHTML =
      `<span class="nm">${nmHtml}` +
        (bits.length ? `<span class="sub">${bits.join(" · ")}</span>` : "") +
      `</span>` +
      (depth === 0 && k.share >= 3
        ? `<span class="bar"><i class="${k.safety}" style="width:${Math.min(100, k.share)}%"></i></span>`
        : `<span class="bar"></span>`) +
      `<span class="pc">${k.share.toFixed(1)}%</span>` +
      `<span class="sz">${human(k.size)}</span>` +
      `<span class="act">${act}</span>`;

    host.appendChild(row);

    const cb = row.querySelector("input[type=checkbox]");
    if (cb) {
      if (picked.has(k.path)) cb.checked = true;
      cb.addEventListener("change", e => {
        if (e.target.checked) picked.add(k.path); else picked.delete(k.path);
        updateDock();
      });
    }

    // 继续往下（它自己也超阈值）
    if (k.children && k.children.length) {
      const det = document.createElement("details");
      det.className = "kids";
      const sum = document.createElement("summary");
      sum.className = "kids-hd";
      sum.innerHTML = `<span class="arrow">▶</span>继续展开（${k.children.length} 项）`;
      const inner = document.createElement("div");
      renderKids(k.children, inner, depth + 1);
      det.appendChild(sum); det.appendChild(inner);
      det.addEventListener("toggle", () => sum.classList.toggle("open", det.open));
      host.appendChild(det);
    }
  });
}

// 勾选集合里既有顶层条目、也有细分出来的子项路径。
// 用一个路径索引把两者都能查到 —— 只查 DATA.items 会漏掉子项。
const ALL_BY_PATH = (() => {
  const m = new Map();
  const walk = list => list.forEach(i => {
    m.set(i.path, i);
    if (i.children) walk(i.children);
  });
  walk(DATA.items);
  return m;
})();
function pickedItems() {
  return [...picked].map(p => ALL_BY_PATH.get(p)).filter(Boolean);
}

function updateDock() {
  const its = pickedItems();
  const total = its.reduce((a, b) => a + b.size, 0);
  const stat = document.getElementById("dock-stat");
  const dock = document.querySelector(".dock");
  if (its.length === 0) {
    stat.textContent = `尚未选择 · 共 ${visible.length} 条可见`;
    dock.classList.remove("active");
  } else {
    stat.innerHTML = `已选 <b>${its.length}</b> 项 ｜ 合计 <b>${human(total)}</b>`;
    dock.classList.add("active");
  }
  const btn = document.getElementById("export");
  btn.disabled = its.length === 0;
  document.getElementById("dock-hint").textContent = its.length
    ? "导出后用 diskdoctor clean --from-selection 执行"
    : "";
}

// 点路径 → **直接**在资源管理器中打开（不再需要点第二次）。
//
// 目录：打开该目录
// 文件：打开它所在的文件夹并选中它 —— 而不是用默认程序打开文件本身。
//       `.docx` / `.exe` 这类一旦双击就会启动别的程序，太重。
//
// 其它操作：
//   右键 / Ctrl+点击   只复制路径，不打开
//
// 没有本地服务时（静态模式）无法直接打开文件夹 —— 这时才弹浮层，
// 给出可粘贴的等效命令。**不留一个点了没反应的按钮。**
let popEl = null;
let toastTimer = null;

function toast(msg) {
  let el = document.getElementById("toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "toast";
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 2800);
}

function ensurePop() {
  if (popEl) return popEl;
  const d = document.createElement("div");
  d.className = "pathpop";
  d.hidden = true;
  d.innerHTML =
    `<div class="pp-path"></div>` +
    `<div class="pp-kind"></div>` +
    `<div class="pp-act"></div>` +
    `<div class="pp-hint"></div>`;
  // 浮层内的点击不该冒泡到 document 的"点外部关闭"逻辑
  d.addEventListener("click", e => e.stopPropagation());
  document.body.appendChild(d);
  popEl = d;
  return d;
}

function hidePop() {
  if (popEl) popEl.hidden = true;
}

function copyText(text, okLabel, btn) {
  const done = () => {
    if (!btn) return;
    const old = btn.textContent;
    btn.textContent = okLabel;
    setTimeout(() => { btn.textContent = old; }, 1400);
  };
  navigator.clipboard.writeText(text).then(done).catch(() => {
    // 剪贴板不可用（file:// 下的权限限制常见）时退化为选中文本
    const r = document.createRange();
    r.selectNodeContents(ensurePop().querySelector(".pp-path"));
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(r);
    if (btn) {
      btn.textContent = "已选中，请按 Ctrl+C";
      setTimeout(() => { btn.textContent = "复制路径"; }, 2200);
    }
  });
}

// 用 <img> 当信标把动作发给本地服务。
// 选它而不是 fetch 的原因：资源加载不受 CORS 限制，请求一定会发出，
// 也不需要读响应（服务返回 204）—— 换成 fetch 反而会在跨源时被拦。
function beacon(action, path) {
  if (!SERVER) return false;
  const url = `http://127.0.0.1:${SERVER.port}/act` +
    `?a=${encodeURIComponent(action)}` +
    `&t=${encodeURIComponent(SERVER.token)}` +
    `&p=${encodeURIComponent(path)}` +
    `&_=${Date.now()}`;
  const im = new Image();
  im.src = url;
  return true;
}

// ---------- 服务健康检查 ----------
//
// 为什么需要：服务会**主动消失** —— 10 分钟无操作自动停止，或者用户把
// 那个进程关了。这时点路径什么都不会发生，而用户不知道原因，
// 只会以为功能坏了。
//
// 判断方式用 `<img>` 的 onload/onerror：服务在 ping 上返回一张真实的 1×1 图，
// 所以 onload == "服务真的应答了"。**不能用非图片响应做探针** ——
// 204、403、以及代理返回的 502 都会触发 onerror，区分不出来。
//
// 这也顺带覆盖了"系统代理劫持了 127.0.0.1"的情况：那种环境下拿到的是代理的
// 502，onerror 会把界面正确切到降级模式，而不是留个点了没反应的路径。
let SERVICE_UP = null;      // null = 还在探测
function pingService() {
  if (!SERVER) { SERVICE_UP = false; return; }
  const probe = new Image();
  let settled = false;
  const done = up => {
    if (settled) return;
    settled = true;
    SERVICE_UP = up;
    if (!up) toast("本地服务未响应，点路径改为「复制路径」模式");
  };
  probe.onload = () => done(true);
  probe.onerror = () => done(false);
  probe.src = `http://127.0.0.1:${SERVER.port}/act?a=ping` +
    `&t=${encodeURIComponent(SERVER.token)}&_=${Date.now()}`;
  // 兜底：2.5 秒还没结果就当不可达（卡住的代理不会立刻报错）
  setTimeout(() => done(false), 2500);
}
if (SERVER) pingService();


function positionPop(d, anchor) {
  const r = anchor.getBoundingClientRect();
  const dr = d.getBoundingClientRect();
  let top = r.bottom + 6;
  if (top + dr.height > window.innerHeight - 8) {
    top = Math.max(8, r.top - dr.height - 6);
  }
  let left = Math.min(r.left, window.innerWidth - dr.width - 12);
  left = Math.max(12, left);
  d.style.top = top + "px";
  d.style.left = left + "px";
}

function showPathPop(anchor, path, isDir) {
  const d = ensurePop();
  const live = SERVER && SERVICE_UP === true;
  d.querySelector(".pp-path").textContent = path;
  d.querySelector(".pp-kind").textContent = (isDir ? "目录" : "文件") +
    (live ? "" : "　·　本地服务未响应，只能复制路径");

  const act = d.querySelector(".pp-act");
  const hint = d.querySelector(".pp-hint");
  act.replaceChildren();
  hint.replaceChildren();

  const mk = (label, cls, fn) => {
    const b = document.createElement("button");
    b.textContent = label;
    if (cls) b.className = cls;
    b.addEventListener("click", e => {
      e.stopPropagation();
      fn(b);
    });
    act.appendChild(b);
    return b;
  };

  if (live) {
    mk(isDir ? "在资源管理器中打开" : "打开所在文件夹", "primary", () => {
      beacon("open", path);
      hidePop();
    });
    if (!isDir) {
      mk("在文件夹中定位", "", () => {
        beacon("reveal", path);
        hidePop();
      });
    }
  }

  mk("复制路径", live ? "" : "primary", b => copyText(path, "已复制", b));

  if (!live) {
    // 服务不可达时给一条能直接粘贴的命令，而不是一个死按钮。
    // 分两种情况说清楚，因为对策不同：
    //   · 压根没启动服务（静态模式）
    //   · 服务曾经在，但已停止（空闲超时 / 被关掉）
    const cmd = `explorer "${path}"`;
    const why = SERVER
      ? "本地服务已停止（可能是闲置太久自动退出，或那个进程被关了）。" +
        "重新打开一次盘点即可恢复。"
      : "当前是无服务的静态界面。";
    hint.innerHTML = why + " 想直接打开的话，把这条命令粘到 " +
      "<b>Win+R</b> 运行框：<br><code>" + esc(cmd) + "</code>";
  }

  d.hidden = false;
  positionPop(d, anchor);
}

// 点路径 → 打开（事件委托，一处管住所有路径）
document.addEventListener("click", e => {
  const t = e.target.closest(".pth");
  if (!t) { hidePop(); return; }

  const path = t.dataset.path || "";
  const isDir = t.dataset.dir === "1";
  if (!path) return;

  e.preventDefault();
  e.stopPropagation();

  // Ctrl / Cmd + 点击：只想要路径，不要打开窗口
  if (e.ctrlKey || e.metaKey) {
    copyText(path, "路径已复制", null);
    return;
  }

  if (SERVER && SERVICE_UP === true) {
    // 有服务：直接打开，不再要第二次点击
    if (beacon(isDir ? "open" : "reveal", path)) {
      toast(isDir ? `已打开：${path}` : `已定位到：${path}`);
    }
  } else {
    // 静态模式，或服务已不可达 —— 开不了文件夹，弹浮层给替代方案。
    // **不留一个点了没反应的路径。**
    showPathPop(t, path, isDir);
  }
});

// 右键 → 复制路径（不弹系统菜单，也不打开窗口）
document.addEventListener("contextmenu", e => {
  const t = e.target.closest(".pth");
  if (!t) return;
  e.preventDefault();
  const path = t.dataset.path || "";
  if (path) copyText(path, "路径已复制", null);
});

// 滚动 / 改窗口大小时浮层会脱离原位置，直接关掉更干净
window.addEventListener("scroll", hidePop, true);
window.addEventListener("resize", hidePop);
document.addEventListener("keydown", e => {
  if (e.key === "Escape") hidePop();
});

// ---------- 操作 ----------
document.getElementById("q").addEventListener("input", render);
catSel.addEventListener("change", render);
document.getElementById("sort").addEventListener("change", render);

// 复制命令：事件委托绑定一次，避免为每个按钮单独绑，也避免重复注册
document.addEventListener("click", e => {
  const b = e.target.closest("button.copy");
  if (!b) return;
  const cmd = b.dataset.cmd || "";
  const orig = "复制命令";
  navigator.clipboard.writeText(cmd).then(() => {
    b.textContent = "已复制";
    setTimeout(() => { b.textContent = orig; }, 1200);
  }).catch(() => {
    // 剪贴板不可用（例如 file:// 下的权限限制）时退化为选中文本
    const code = b.previousElementSibling;
    if (code) {
      const r = document.createRange();
      r.selectNodeContents(code);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(r);
    }
    b.textContent = "已选中，请 Ctrl+C";
    setTimeout(() => { b.textContent = orig; }, 2000);
  });
});

document.getElementById("pick-safe").addEventListener("click", () => {
  // 只选「可安全回收」—— 这是唯一无需逐个判断的一档。
  // 「需确认」刻意不自动选：那一档的意思就是"你得看一眼"。
  //
  // 也覆盖细分出来的子项：用户在展开的区域里看到某块可回收，
  // "全选可安全回收"却不选它，会让人以为哪里不对。
  const walk = list => list.forEach(i => {
    if (i.safety === "safe" && i.selectable && i.path) picked.add(i.path);
    if (i.children) walk(i.children);
  });
  walk(DATA.items);
  render();
});

document.getElementById("clear").addEventListener("click", () => {
  picked.clear();
  render();
});

document.getElementById("export").addEventListener("click", () => {
  const its = pickedItems();
  const payload = {
    tool: "diskdoctor",
    format: 1,
    generated_at: new Date().toISOString(),
    scan_root: DATA.root,
    total_bytes: its.reduce((a, b) => a + b.size, 0),
    items: its.map(i => ({
      path: i.path, size: i.size, safety: i.safety,
      rule_id: i.rule_id, rule_name: i.rule_name,
    })),
  };
  const blob = new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  a.download = `diskdoctor-selection-${stamp}.json`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
});

render();

</script>
</body>
</html>
"##;
