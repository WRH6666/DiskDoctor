//! DiskDoctor 命令行入口。
//!
//! 两条主线：
//!
//! - **只读**：`diskdoctor <路径>` 扫描并出报告，不碰任何文件
//! - **清理**：`diskdoctor clean` 把文件移进暂存区（可撤销），
//!   `diskdoctor trash` 管理暂存区
//!
//! 清理刻意拆成两个命令，中间留出反悔的时间 —— 见 `trash_cmd` 的说明。

mod explain_cmd;
mod serve;
mod survey;
mod trash_cmd;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dd_analyze::{human_size, Analysis};
use dd_core::Safety;
use dd_exec::trash::StageRequest;
use dd_rules::Ruleset;
use dd_scanner::ScanOptions;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "diskdoctor",
    version,
    about = "磁盘空间体检：看清楚空间被谁吃了，以及每一项能不能删",
    long_about = "DiskDoctor 分两步走：\n  \
        1. 只读体检 —— 扫描并告诉你空间去哪了、每一项能不能删\n  \
        2. 安全清理 —— 把文件移进暂存区（可撤销），确认后再真正释放\n\n\
        不指定子命令时执行只读体检。"
)]
struct Cli {
    /// 要扫描的路径（默认当前目录）。指定子命令时忽略本参数。
    ///
    /// 这里刻意**不给 default_value**：有默认值会让 clap 认为本参数总是存在，
    /// 从而把 `diskdoctor survey X` 里的 `survey` 当成路径吃掉，
    /// 导致子命令永远进不去。默认值改到运行时补。
    #[arg(value_name = "PATH")]
    path: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,

    /// 把 Markdown 报告写到文件
    #[arg(long, value_name = "FILE", global = true)]
    out: Option<PathBuf>,

    /// 把完整分析结果以 JSON 写出
    #[arg(long, value_name = "FILE", global = true)]
    json: Option<PathBuf>,

    /// 禁用 NTFS MFT 直读，强制使用并行遍历
    #[arg(long, global = true)]
    no_mft: bool,

    /// 扫描时跳过的目录名，可重复，如 --exclude node_modules
    #[arg(long = "exclude", value_name = "NAME", global = true)]
    excludes: Vec<String>,

    /// 报告里每档列出的条目数
    #[arg(long, default_value_t = 20, global = true)]
    top: usize,

    /// 只做规则库自检，不扫描
    #[arg(long, global = true)]
    check_rules: bool,

    /// 列出规则库内容
    #[arg(long, global = true)]
    list_rules: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 生成可浏览的决策界面：看清装了什么、能否删，并自己挑选
    Survey(survey::SurveyArgs),

    /// 把「可安全回收」的项目移入暂存区（可撤销，不是删除）
    Clean(CleanArgs),

    /// 管理暂存区：查看、恢复、彻底清除
    Trash(trash_cmd::TrashArgs),

    /// 解释一条路径被归到哪条规则、为什么 —— 用于核对清单是否可信
    Explain(ExplainArgs),
}

#[derive(clap::Args, Debug)]
struct ExplainArgs {
    /// 要分析的路径
    path: String,

    /// 按文件判定（默认按目录判定）
    #[arg(long)]
    file: bool,
}

#[derive(clap::Args, Debug)]
struct CleanArgs {
    /// 要清理的路径（在其中寻找可回收项）。用 --from-selection 时可省略。
    path: Option<String>,

    /// 按 `survey` 界面导出的选择文件执行清理
    ///
    /// 这是产品的主路径：先在界面上看清、挑选，再用这里的清单执行。
    /// 注意选择文件只是**意图** —— 执行时每一项都会重新过护栏与规则判定，
    /// 所以手工往清单里塞受保护路径是无效的。
    #[arg(long, value_name = "FILE")]
    from_selection: Option<PathBuf>,

    /// 只预览会清理什么，不移动任何文件
    #[arg(long)]
    dry_run: bool,

    /// 实际执行，不用再确认
    #[arg(long)]
    yes: bool,

    /// 按类别名筛选，可重复。如 --category 包管理器缓存 --category 浏览器缓存
    #[arg(long = "category", value_name = "NAME")]
    categories: Vec<String>,

    /// 只清理大于 N MB 的项目
    #[arg(long, value_name = "MB", default_value_t = 0)]
    min_mb: u64,

    /// 每批最多处理多少项（防止一次动太多）
    #[arg(long, default_value_t = 200)]
    limit: usize,

    /// 小于这个体积（MB）的条目，若能被上层目录代表，就不单独列出。
    ///
    /// 存在的理由：一次实测里 283 个候选中，有 **121 个挤在用户文档区、
    /// 合计只有 35 MB**（平均每条 0.3 MB）—— 它们不贡献空间，
    /// 却占了清单 43% 的行数，把真正的大头淹没了。
    ///
    /// 这**不会漏掉空间**：被折叠的条目要么已被上层目录代表，要么
    /// 会以聚合形式出现在"其它 N 项"里。设为 0 可关闭此行为。
    #[arg(long, default_value_t = 8)]
    merge_below_mb: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let rules = Ruleset::embedded().context("内嵌规则库加载失败")?;

    if cli.check_rules || cli.list_rules {
        print_rules(&rules, cli.list_rules);
        return Ok(());
    }

    let scan_opts = ScanOptions {
        try_mft: !cli.no_mft,
        exclude_names: cli.excludes.iter().map(|s| s.to_ascii_lowercase()).collect(),
        follow_links: false,
    };

    match &cli.command {
        Some(Cmd::Survey(args)) => return survey::run(args, &rules, &scan_opts),
        Some(Cmd::Trash(args)) => return trash_cmd::run(args),
        Some(Cmd::Clean(args)) => return run_clean(args, &rules, &scan_opts),
        Some(Cmd::Explain(args)) => return explain_cmd::run(&args.path, &rules, !args.file),
        None => run_scan(&cli, &rules, &scan_opts),
    }
}

// ------------------------------------------------------------------ 只读体检

fn run_scan(cli: &Cli, rules: &Ruleset, opts: &ScanOptions) -> Result<()> {
    // 顶层没给路径时默认扫当前目录（默认值补在运行时而非 clap，
    // 否则 clap 会把子命令名当成路径吃掉）
    let target = cli.path.clone().unwrap_or_else(|| ".".to_string());
    let path = std::fs::canonicalize(&target)
        .with_context(|| format!("路径不存在或无法访问: {target}"))?;
    let path = strip_verbatim(path);

    println!("DiskDoctor · 只读分析模式");
    println!("扫描路径 : {}", path.display());
    println!("规则条数 : {}", rules.len());
    println!("开始扫描 …");

    let t0 = std::time::Instant::now();
    let snapshot = dd_scanner::scan_with_specials(&path, opts).context("扫描失败")?;
    let scan_ms = t0.elapsed().as_millis();

    println!(
        "扫描完成 : {} 个文件 / {} 个目录，用时 {:.1}s",
        snapshot.file_count(),
        snapshot.dir_count(),
        scan_ms as f64 / 1000.0
    );

    let backend = if opts.try_mft {
        "并行目录遍历（MFT 直读尚未接入，见 dd-scanner/src/ntfs.rs）"
    } else {
        "并行目录遍历"
    };

    let analysis = Analysis::run(snapshot, rules.clone(), backend);
    let volumes = dd_scanner::special::volume_infos();
    let report = analysis.build_report(volumes);

    let specials_total: u64 = report
        .specials
        .iter()
        .filter(|s| s.ok && s.size > 0)
        .map(|s| s.size)
        .sum();

    println!();
    println!("──────────────────────────────────────────");
    println!("可安全回收 : {}", human_size(report.confident_reclaim()));
    println!("需人工确认 : {}", human_size(report.caution_size));
    println!(
        "系统级占用 : {}",
        if specials_total > 0 {
            human_size(specials_total)
        } else {
            "未采集到（需要管理员权限）".to_string()
        }
    );
    println!("个人数据   : {}", human_size(report.keep_size));
    println!("未识别     : {}", human_size(report.unattributed));
    println!("──────────────────────────────────────────");

    println!();
    println!("可安全回收 TOP {}:", report.top_safe.len().min(cli.top));
    for f in report.top_safe.iter().take(cli.top) {
        println!(
            "  {:>10}  {:<22} {}",
            human_size(f.size),
            f.rule_name,
            f.path
        );
    }

    if !report.specials.is_empty() {
        println!();
        println!("系统级占用:");
        for s in &report.specials {
            let size = if s.ok {
                human_size(s.size)
            } else {
                "未能读取".to_string()
            };
            println!("  {:>10}  {:<18} {}", size, s.name, s.action_text());
        }
    }

    if let Some(p) = &cli.out {
        std::fs::write(p, report.to_markdown()).with_context(|| format!("写入 {} 失败", p.display()))?;
        println!();
        println!("Markdown 报告已写入: {}", p.display());
    }
    if let Some(p) = &cli.json {
        std::fs::write(p, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("写入 {} 失败", p.display()))?;
        println!("JSON 结果已写入   : {}", p.display());
    }

    println!();
    if report.confident_reclaim() > 0 {
        println!(
            "想回收那 {} 的话：diskdoctor clean \"{}\" --dry-run",
            human_size(report.confident_reclaim()),
            path.display()
        );
    }
    println!("本次运行为只读分析，没有删除或修改任何文件。");
    Ok(())
}

// ------------------------------------------------------------------ 清理

fn run_clean(args: &CleanArgs, rules: &Ruleset, opts: &ScanOptions) -> Result<()> {
    // 主路径：从 survey 界面导出的选择文件执行
    if let Some(sel_file) = &args.from_selection {
        return apply_selection(sel_file, rules, args.dry_run, args.yes);
    }

    let Some(path_arg) = args.path.as_ref() else {
        anyhow::bail!(
            "请指定要清理的路径（例如 diskdoctor clean \"C:\\Users\\me\"），\n\
             或用 --from-selection 指定从 survey 界面导出的选择文件。\n\n\
             提示：先跑 `diskdoctor survey <路径>` 生成可浏览的界面，\
             在里面逐条挑选后再执行，比盲扫更可控。"
        );
    };

    let path = std::fs::canonicalize(path_arg)
        .with_context(|| format!("路径不存在或无法访问: {path_arg}"))?;
    let path = strip_verbatim(path);

    println!("DiskDoctor · 清理模式");
    println!("扫描路径 : {}", path.display());

    // 不采集系统级特殊项 —— 那些不能通过暂存区处理
    let snapshot = dd_scanner::scan(&path, opts).context("扫描失败")?;
    let analysis = Analysis::run(snapshot, rules.clone(), "并行目录遍历");

    // —— 挑选候选 ——
    //
    // 只取 `safe` 级。`caution` 及以上的从不自动进入清理清单，
    // 因为它们要么可能含用户数据，要么是系统托管。这是硬规则，
    // 不是默认值 —— 没有 `--force-safety` 这种东西。
    let mut candidates: Vec<&dd_analyze::Finding> = analysis
        .findings
        .iter()
        .filter(|f| f.safety == Safety::Safe)
        .filter(|f| f.size >= args.min_mb.saturating_mul(1024 * 1024))
        .filter(|f| {
            args.categories.is_empty()
                || args
                    .categories
                    .iter()
                    .any(|c| f.category_label.contains(c.as_str()) || f.rule_name.contains(c.as_str()))
        })
        .collect();

    candidates.sort_by(|a, b| b.size.cmp(&a.size));

    // —— 分流：逐条列出 / 聚合 / 排除 ——
    //
    // 一次真实扫描的原始候选是 283 项，但其中 121 项挤在用户文档区、
    // 合计仅 35 MB（平均 0.3 MB），把真正的大头淹没了。所以按体积分流：
    //
    // ① **被祖先覆盖**的小条目 —— 不额外占空间，纯噪音，直接摘掉
    // ② **用户文档区的小条目** —— 排除。那是你自己的文件区，
    //    为几十 MB 去动上百个目录，风险收益完全不成比例
    // ③ **其余小条目** —— 仍然清理，只是不逐条列出而是聚合成一行。
    //    这样既不刷屏也不漏空间
    //
    // `--merge-below-mb 0` 可关闭全部分流，看到完整清单。
    let merge_threshold = args.merge_below_mb.saturating_mul(1024 * 1024);
    let mut covered_count = 0usize;
    let mut parked: Vec<&dd_analyze::Finding> = Vec::new();

    if merge_threshold > 0 {
        // ① 被祖先覆盖的（先克隆路径，避免与 retain 的可变借用冲突）
        let all_paths: Vec<String> = candidates.iter().map(|f| f.path.clone()).collect();
        let before = candidates.len();
        candidates.retain(|f| {
            if f.size >= merge_threshold {
                return true;
            }
            let prefix = format!("{}\\", f.path);
            !all_paths.iter().any(|p| p.starts_with(&prefix))
        });
        covered_count = before - candidates.len();

        // ② 用户文档区的小条目
        let mut keep: Vec<&dd_analyze::Finding> = Vec::with_capacity(candidates.len());
        for f in candidates.drain(..) {
            if f.size < merge_threshold && is_user_workspace(&f.path) {
                parked.push(f);
            } else {
                keep.push(f);
            }
        }
        candidates = keep;
    }

    let total_found = candidates.len();
    let total_size: u64 = candidates.iter().map(|f| f.size).sum();

    // ③ 按体积分流为「逐条列出」与「聚合」
    let mut listed: Vec<&dd_analyze::Finding> = Vec::new();
    let mut lumped: Vec<&dd_analyze::Finding> = Vec::new();
    for (i, f) in candidates.iter().enumerate() {
        let too_small = merge_threshold > 0 && f.size < merge_threshold;
        if i >= args.limit || too_small {
            lumped.push(f);
        } else {
            listed.push(f);
        }
    }
    let lumped_size: u64 = lumped.iter().map(|f| f.size).sum();

    if listed.is_empty() && lumped.is_empty() {
        println!();
        println!("没有找到符合条件、且可安全回收的项目。");
        if args.min_mb > 0 {
            println!("（筛选条件：大于 {} MB）", args.min_mb);
        }
        if !args.categories.is_empty() {
            println!("（筛选条件：类别包含 {:?}）", args.categories);
        }
        return Ok(());
    }

    println!();
    println!(
        "找到 {} 项可安全回收，共 {}",
        total_found,
        human_size(total_size)
    );
    println!();

    if listed.is_empty() {
        println!("（全是小条目，已聚合显示在下方）");
    } else {
        println!("{:<8} {:>10}  {:<24} 位置", "", "体积", "类别");
        println!("{}", "─".repeat(78));
        for f in &listed {
            // 路径必须完整可读：用户是要**照着这份清单核对**的，
            // 截断成 `AppData…\node_modules` 等于让他无法判断这是什么。
            // 只在极端长（超过 160 字符）时才折中省略中段。
            println!(
                "{:<8} {:>10}  {:<24} {}",
                "  ·",
                human_size(f.size),
                f.category_label,
                shorten_path(&f.path, 160)
            );
        }
        println!("{}", "─".repeat(78));
    }

    if !lumped.is_empty() {
        println!(
            "（另有 {} 项较小的缓存条目，合计 {}，已**一并纳入本次清理**，仅未逐条列出）",
            lumped.len(),
            human_size(lumped_size)
        );
    }
    if covered_count > 0 {
        println!("（已忽略 {} 个被上层目录覆盖的条目，它们不额外占空间）", covered_count);
    }
    if !parked.is_empty() {
        let ps: u64 = parked.iter().map(|f| f.size).sum();
        println!();
        println!(
            "⚠ 已**排除** {} 项位于用户文档区的小条目（合计 {}），不纳入本次清理。",
            parked.len(),
            human_size(ps)
        );
        println!("  理由：那是你自己的工作文件区，单个都很小、删了也省不下什么，");
        println!("       但一次动上百个目录，风险和收益完全不成比例。");
        println!("  想看看它们：加 --merge-below-mb 0 展开全部。");
    }

    let reqs: Vec<StageRequest> = listed
        .iter()
        .chain(lumped.iter())
        .map(|f| {
            StageRequest::new(&f.path)
                .reason(format!("{} — {}", f.category_label, f.rule_name))
                .rule(&f.rule_id)
                .size(f.size)
        })
        .collect();

    if args.dry_run {
        println!();
        println!("[试运行] 未移动任何文件。去掉 --dry-run 即执行。");
        println!("提示：执行后文件进入暂存区，可用 `diskdoctor trash restore` 撤销。");
        return Ok(());
    }

    if !args.yes {
        println!();
        println!("以上 {} 项将被**移入暂存区**（不是删除，可撤销）。", reqs.len());
        println!("确认请加 --yes 参数重新执行。");
        println!("想先看看效果：加 --dry-run");
        return Ok(());
    }

    println!();
    let note = format!("clean {} @ {}", path.display(), reqs.len());
    trash_cmd::stage_paths(&reqs, &note, false)?;
    Ok(())
}

// ------------------------------------------------------------------ 按选择清理

/// `survey` 界面导出的选择文件。
#[derive(serde::Deserialize)]
struct SelectionFile {
    #[serde(default)]
    tool: String,
    #[serde(default)]
    scan_root: String,
    #[serde(default)]
    items: Vec<SelectionItem>,
}

#[derive(serde::Deserialize)]
struct SelectionItem {
    path: String,
    #[serde(default)]
    size: u64,
    /// 导出时界面上的判定，用于和执行时的重新判定做比对
    #[serde(default)]
    safety: String,
    /// 导出时命中的规则 ID / 名称。
    ///
    /// 这两个字段**不参与执行决策** —— 执行时一律重新判定（见下面
    /// `apply_selection` 的说明），界面当时的结论只作参考。保留它们是为了：
    ///
    /// 1. 与界面导出的 JSON 结构保持一致（字段对不上时用户会以为是格式变了）
    /// 2. 将来判定结果不一致时，能指出「当时是按哪条规则判的」
    ///
    /// 所以这里显式声明允许未读取，而不是删掉让人以为选择文件里没有它。
    #[serde(default)]
    #[allow(dead_code)]
    rule_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    rule_name: String,
}

/// 按用户的选择执行清理。
///
/// # 为什么每一项都要重新验证
///
/// 选择文件是**外部输入** —— 它可能：
/// - 被用户手工编辑过（有意或无意地塞进 `C:\Windows` 这类路径）
/// - 是几天前生成的，期间规则库已更新（某条判定从 safe 变严）
/// - 对应的文件已经不存在、或者被别的程序替换了
///
/// 所以用户勾选表达的是**意图**，不是授权。工具必须自己再判一次：
/// 能清理的才清理，判定变严的拒绝并说明原因。
/// 最后还有一道硬护栏（在 `stage` 里）兜底。
///
/// 这个设计的好处是：**即使清单被篡改，也删不掉不该删的东西。**
fn apply_selection(file: &Path, rules: &Ruleset, dry_run: bool, yes: bool) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("读取选择文件失败: {}", file.display()))?;
    let sel: SelectionFile = serde_json::from_str(&text).with_context(|| {
        format!(
            "解析 {} 失败 —— 它应该是 `diskdoctor survey` 界面里导出的 JSON",
            file.display()
        )
    })?;

    if !sel.tool.is_empty() && sel.tool != "diskdoctor" {
        anyhow::bail!(
            "这个文件不是 DiskDoctor 导出的（tool = \"{}\"）",
            sel.tool
        );
    }
    if sel.items.is_empty() {
        println!("选择文件里没有任何条目，无需清理。");
        return Ok(());
    }

    println!("DiskDoctor · 按选择清理");
    println!("选择文件 : {}", file.display());
    if !sel.scan_root.is_empty() {
        println!("扫描来源 : {}", sel.scan_root);
    }
    println!("勾选条目 : {} 项", sel.items.len());
    println!();

    // —— 逐条重新验证 ——
    //
    // 三件事：去重、去掉被祖先覆盖的、重新判定。
    // 顺序不能反：先做完集合运算，再对最终清单判定，避免做了无用功
    // （也避免"父项被选中后子项已不存在"这种噪音）。
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unique: Vec<&SelectionItem> = Vec::with_capacity(sel.items.len());
    for it in &sel.items {
        let key = it.path.to_ascii_lowercase();
        if seen.insert(key) {
            unique.push(it);
        }
    }
    let dup_n = sel.items.len() - unique.len();

    // 去掉被祖先覆盖的项：同一目录会同时出现在顶层列表和某个大条目的细分里，
    // 用户可能在两处都勾上。移动父目录时子项已被一并带走，
    // 再单独处理它只会报"已不存在" —— 不是错误，但会让报告出现无谓的失败项。
    let all_paths: Vec<String> = unique
        .iter()
        .map(|i| i.path.trim_end_matches('\\').to_ascii_lowercase())
        .collect();
    let mut kept: Vec<&SelectionItem> = Vec::with_capacity(unique.len());
    let mut covered: Vec<&SelectionItem> = Vec::new();
    for (idx, it) in unique.iter().enumerate() {
        let me = &all_paths[idx];
        let has_ancestor = all_paths.iter().enumerate().any(|(j, p)| {
            j != idx
                && me.starts_with(p.as_str())
                && me.len() > p.len()
                && me.as_bytes()[p.len()] == b'\\'
        });
        if has_ancestor {
            covered.push(it);
        } else {
            kept.push(it);
        }
    }

    let mut accepted: Vec<(String, u64, String)> = Vec::new();
    let mut rejected: Vec<(String, String)> = Vec::new();
    let mut changed: Vec<(String, String, String)> = Vec::new();

    for it in kept.iter().copied() {
        let p = PathBuf::from(&it.path);

        if !p.exists() {
            rejected.push((it.path.clone(), "路径已不存在（可能已被清理或移动）".into()));
            continue;
        }

        let is_dir = p.is_dir();
        let Some(idx) = explain_cmd::resolve_final(&it.path, rules, is_dir) else {
            rejected.push((
                it.path.clone(),
                "规则库无法归因（未识别）—— 不予清理".into(),
            ));
            continue;
        };
        let rule = rules.rule(idx);

        if !matches!(rule.safety, Safety::Safe | Safety::Caution) {
            rejected.push((
                it.path.clone(),
                format!(
                    "当前判定为「{}」（规则 {}），已超出可清理范围 —— 拒绝",
                    rule.safety.label(),
                    rule.id
                ),
            ));
            continue;
        }

        // 判定和导出时不一致 → 提示（规则库更新了，或清单被改过）
        if !it.safety.is_empty() && it.safety != rule.safety.key() {
            changed.push((
                it.path.clone(),
                it.safety.clone(),
                rule.safety.key().to_string(),
            ));
        }

        accepted.push((
            it.path.clone(),
            if it.size > 0 { it.size } else { 0 },
            format!("{} — {}", rule.name, rule.id),
        ));
    }

    if dup_n > 0 || !covered.is_empty() {
        println!("已去掉重复项：");
        if dup_n > 0 {
            println!("  {dup_n} 项在清单里出现了多次（同一路径只处理一次）");
        }
        if !covered.is_empty() {
            println!(
                "  {} 项被已选中的上级目录覆盖（移动父目录时会一并带走）：",
                covered.len()
            );
            for c in covered.iter().take(6) {
                println!("    · {}", c.path);
            }
            if covered.len() > 6 {
                println!("    … 另有 {} 项", covered.len() - 6);
            }
        }
        println!();
    }

    if !changed.is_empty() {
        println!("注意：以下条目当前的判定与导出时**不一致**：");
        for (p, was, now) in &changed {
            println!("  · {p}");
            println!("      {was} → {now}");
        }
        println!("  （可能是规则库已更新，或清单被编辑过。以当前判定为准。）");
        println!();
    }

    if accepted.is_empty() {
        println!("没有任何条目通过校验，因此什么也不会做。");
        if !rejected.is_empty() {
            println!();
            println!("被拒绝的 {} 项：", rejected.len());
            for (p, why) in &rejected {
                println!("  ✗ {p}");
                println!("      {why}");
            }
        }
        return Ok(());
    }

    let total: u64 = accepted.iter().map(|(_, s, _)| *s).sum();

    println!("通过校验，将清理 {} 项：", accepted.len());
    println!("{}", "─".repeat(78));
    for (p, s, why) in accepted.iter().take(30) {
        println!("  {:>10}  {}", human_size(*s), p);
        println!("              ← {why}");
    }
    if accepted.len() > 30 {
        println!("  … 另有 {} 项", accepted.len() - 30);
    }
    println!("{}", "─".repeat(78));
    println!("合计：{}", human_size(total));

    if !rejected.is_empty() {
        println!();
        println!("被拒绝的 {} 项（不会处理）：", rejected.len());
        for (p, why) in rejected.iter().take(15) {
            println!("  ✗ {p}");
            println!("      {why}");
        }
        if rejected.len() > 15 {
            println!("  … 另有 {} 项", rejected.len() - 15);
        }
    }

    if dry_run {
        println!();
        println!("[试运行] 未移动任何文件。去掉 --dry-run 即执行。");
        return Ok(());
    }

    if !yes {
        println!();
        println!("以上 {} 项将被**移入暂存区**（不是删除，可撤销）。", accepted.len());
        println!("确认请加 --yes 参数重新执行。");
        println!("想先看看效果：加 --dry-run");
        return Ok(());
    }

    let reqs: Vec<dd_exec::StageRequest> = accepted
        .iter()
        .map(|(p, s, why)| {
            dd_exec::StageRequest::new(p)
                .reason(why.clone())
                .size(*s)
        })
        .collect();

    println!();
    let note = format!("按选择清理 {} @ {} 项", file.display(), reqs.len());
    trash_cmd::stage_paths(&reqs, &note, false)?;
    Ok(())
}

// ------------------------------------------------------------------ 工具

fn print_rules(rules: &Ruleset, verbose: bool) {
    println!("规则库载入成功，共 {} 条。", rules.len());
    if !verbose {
        println!("加 --list-rules 可查看全部规则。");
        return;
    }
    println!();
    println!("{:<28} {:<20} {:<14} {}", "ID", "名称", "安全等级", "类别");
    println!("{}", "-".repeat(100));
    for r in rules.all() {
        println!(
            "{:<28} {:<20} {:<14} {}",
            r.id,
            truncate(&r.name, 18),
            r.safety.label(),
            r.category.label()
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let mut out: String = chars[..n].iter().collect();
    out.push('…');
    out
}

/// 是否位于用户的个人工作区（文档 / 桌面 / 图片 / 视频 / 下载）。
///
/// 用于判断"要不要折叠小条目" —— 这些目录里的东西，即使技术上可重建，
/// 也是用户自己的文件，不值得为几 MB 去动上百个目录。
fn is_user_workspace(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    ["\\documents\\", "\\desktop\\", "\\pictures\\", "\\videos\\", "\\downloads\\"]
        .iter()
        .any(|k| lower.contains(k))
}

/// 只在路径极长时省略中段，尽量保留头尾（用户靠这两段识别它是什么）。
fn shorten_path(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head_len = max / 2;
    let tail_len = max - head_len - 1;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
}

/// 把「自 1970-01-01 起的天数」换成 (年, 月, 日)。
///
/// Howard Hinnant 的 `civil_from_days` 算法 —— 纯整数运算，无时区依赖，
/// 也不引入 chrono 这种重依赖。`diskdoctor` 对时间的需求只有"显示给人看"，
/// 不值得为此背一个日历库。
pub fn civil_from_days_pub(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Windows 的 `canonicalize` 会返回 `\\?\C:\...` 形式的 verbatim 路径。
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.to_ascii_uppercase().starts_with("UNC\\") {
            return PathBuf::from(rest);
        }
    }
    drop(s);
    p
}
