//! `diskdoctor trash` 子命令：查看、恢复、彻底清除暂存区。
//!
//! 清理分两步走，这个命令管的是后半步：
//!
//! ```text
//!   clean  →  文件进暂存区（可悔改）→  trash list  →  trash purge（真正回收）
//! ```
//!
//! 之所以把 `clean` 和 `purge` 分开成两个命令，是刻意的设计：
//! **中间留出一段时间让你反悔。** 想省事就用
//! `trash purge --older-than 7`，让超过 7 天的自动清掉。

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use dd_analyze::human_size;
use dd_exec::space::DeltaVerdict;
use dd_exec::trash::{all_trashes, Trash};

#[derive(Args, Debug)]
pub struct TrashArgs {
    #[command(subcommand)]
    pub cmd: Option<TrashCmd>,
}

#[derive(Subcommand, Debug)]
pub enum TrashCmd {
    /// 列出暂存区内容（默认只列还占着空间的）
    List {
        /// 连同历史记录一起列出（含已恢复、已清除的）
        #[arg(long)]
        all: bool,
    },

    /// 汇总各个卷上暂存区占用了多少
    Usage,

    /// 恢复文件到原位置
    Restore {
        /// 操作 ID；省略则恢复全部可恢复的
        #[arg(long, value_name = "ID")]
        id: Option<String>,

        /// 只预览会恢复什么，不动文件
        #[arg(long)]
        dry_run: bool,
    },

    /// 彻底删除 —— 不可逆，这一步才真正释放空间
    Purge {
        /// 操作 ID；省略则处理全部
        #[arg(long, value_name = "ID")]
        id: Option<String>,

        /// 只清除 N 天前的批次（推荐用于定期回收）
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,

        /// 只预览会释放多少，不删任何东西
        #[arg(long)]
        dry_run: bool,

        /// 跳过二次确认
        #[arg(long)]
        yes: bool,
    },

    /// 清除历史记录（只删已不再占用空间的批次，里面没有文件）
    ClearHistory {
        /// 只预览会清除多少条记录
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(args: &TrashArgs) -> Result<()> {
    match &args.cmd {
        None | Some(TrashCmd::List { all: false }) => list(false),
        Some(TrashCmd::List { all: true }) => list(true),
        Some(TrashCmd::Usage) => usage(),
        Some(TrashCmd::Restore { id, dry_run }) => restore(id.as_deref(), *dry_run),
        Some(TrashCmd::Purge {
            id,
            older_than,
            dry_run,
            yes,
        }) => purge(id.as_deref(), *older_than, *dry_run, *yes),
        Some(TrashCmd::ClearHistory { dry_run }) => clear_history(*dry_run),
    }
}

fn clear_history(dry_run: bool) -> Result<()> {
    let trashes = all_trashes();
    if trashes.is_empty() {
        println!("没有找到暂存区。");
        return Ok(());
    }

    let mut total_n = 0usize;
    let mut total_bytes = 0u64;
    for t in &trashes {
        let (n, bytes) = t.clear_history(dry_run)?;
        if n == 0 {
            continue;
        }
        total_n += n;
        total_bytes += bytes;
        println!(
            "{}  {} 条记录，{}",
            t.volume().display(),
            n,
            human_size(bytes)
        );
    }

    println!();
    if total_n == 0 {
        println!("没有可清除的历史记录。");
        println!("（还占着空间的批次不会被清除，请先用 purge 处理）");
    } else if dry_run {
        println!("[预览] 将清除 {total_n} 条历史记录，释放 {}", human_size(total_bytes));
        println!("去掉 --dry-run 即执行。");
    } else {
        println!("已清除 {total_n} 条历史记录，释放 {}", human_size(total_bytes));
        println!("（这些记录里本来就没有文件，所以释放的是元数据，不是你的数据）");
    }
    Ok(())
}

fn list(include_history: bool) -> Result<()> {
    let trashes = all_trashes();
    if trashes.is_empty() {
        println!("暂存区是空的 —— 所有卷上都没有 DiskDoctor 的暂存目录。");
        println!();
        println!("提示：先跑一次 `diskdoctor clean <路径>`，文件才会进入暂存区。");
        return Ok(());
    }

    let mut total_entries = 0usize;
    for t in &trashes {
        let entries = t.list(include_history)?;
        if entries.is_empty() {
            continue;
        }
        println!("卷 {}  暂存区 {}", t.volume().display(), t.root().display());
        println!("{}", "─".repeat(74));

        for e in &entries {
            total_entries += 1;
            let occupy = if e.status.occupies_space() {
                format!("占 {:>10}", human_size(e.remaining()))
            } else {
                format!("{:>13}", "—")
            };
            let skipped_n = e.skipped_items().count();
            println!(
                "  {}  {}  {}  {} 项  {}{}",
                e.id,
                fmt_time(e.created_at),
                occupy,
                e.staged_count(),
                e.status.label(),
                if skipped_n > 0 {
                    format!("（另有 {skipped_n} 项未移入）")
                } else {
                    String::new()
                }
            );
            if !e.note.is_empty() {
                println!("      备注：{}", e.note);
            }
            // 只展示前 8 项，避免刷屏
            for it in e.items.iter().take(8) {
                // 未移入的项要显式标出来 —— 否则用户会以为它已被清理，
                // 而实际上它一直在原处
                let mark = match (&it.skipped_reason, it.restored) {
                    (Some(why), _) => format!(" ← 未移入：{why}"),
                    (None, true) => " ← 已恢复".to_string(),
                    (None, false) => String::new(),
                };
                println!(
                    "        · {:>10}  {}{}",
                    human_size(it.size),
                    it.original_path,
                    if it.reason.is_empty() {
                        String::new()
                    } else {
                        format!("  ← {}", it.reason)
                    },
                );
                if !mark.is_empty() {
                    println!("          {mark}");
                }
            }
            if e.items.len() > 8 {
                println!("        … 还有 {} 项", e.items.len() - 8);
            }
            println!();
        }
        println!();
    }

    if total_entries == 0 {
        println!("暂存区里没有还占着空间的内容。");
    } else {
        println!("用 `diskdoctor trash restore --id <ID>` 恢复；");
        println!("用 `diskdoctor trash purge --older-than 7` 清除 7 天前的批次以真正释放空间。");
    }
    Ok(())
}

fn usage() -> Result<()> {
    let trashes = all_trashes();
    if trashes.is_empty() {
        println!("没有任何卷上存在暂存区。");
        return Ok(());
    }
    println!("{:<8} {:>12} {:>8}   位置", "卷", "占用", "项数");
    println!("{}", "─".repeat(70));
    let mut total = 0u64;
    let mut total_n = 0usize;
    for t in &trashes {
        let (bytes, n) = t.usage()?;
        total += bytes;
        total_n += n;
        println!(
            "{:<8} {:>12} {:>8}   {}",
            t.volume().display(),
            human_size(bytes),
            n,
            t.root().display()
        );
    }
    println!("{}", "─".repeat(70));
    println!("{:<8} {:>12} {:>8}", "合计", human_size(total), total_n);
    println!();
    if total == 0 {
        println!("暂存区为空，没有待回收的空间。");
    } else {
        println!(
            "⚠ 这 {} 目前**还没有真正释放** —— 文件只是从原位置移到了暂存区。",
            human_size(total)
        );
        println!("  要真正回收，请执行：diskdoctor trash purge --older-than 7 --yes");
    }
    Ok(())
}

fn restore(id: Option<&str>, dry_run: bool) -> Result<()> {
    let trashes = all_trashes();
    if trashes.is_empty() {
        println!("没有找到暂存区。");
        return Ok(());
    }

    let mut any = false;
    let mut any_skipped = false;
    let mut restored_n = 0usize;
    for t in &trashes {
        // 指定了 id 时先确认这个卷上有没有它，避免无意义的调用
        if let Some(want) = id {
            let has = t.list(true)?.iter().any(|e| e.id == want);
            if !has {
                continue;
            }
        }
        let outcomes = t.restore(id, dry_run).context("恢复失败")?;
        for o in outcomes {
            any = true;
            let verb = if dry_run { "将恢复" } else { "已恢复" };
            println!("{} 操作 {}", verb, o.op_id);
            println!("  {} 项", o.restored);
            restored_n += o.restored;
            for (path, why) in &o.skipped {
                any_skipped = true;
                println!("  ⚠ 跳过 {path}");
                println!("      {why}");
            }
        }
    }

    if !any {
        if let Some(id) = id {
            anyhow::bail!("找不到操作 {id}");
        }
        println!("没有需要恢复的内容。");
    } else if dry_run {
        println!();
        println!("以上为预览，未改动任何文件。去掉 --dry-run 即执行。");
    }

    // 被跳过的项需要明确的后续指引 —— 否则它们会一直躺在暂存区占空间，
    // 而用户不知道该怎么办。
    if any_skipped && !dry_run {
        println!();
        println!("关于被跳过的项：");
        println!("  它们**没有做任何改动** —— 原位置在清理之后又出现了一份；");
        println!("  这通常是程序自己重建的（缓存类目录被清掉后会被自动重新生成）。");
        println!("  暂存区里留着的是清理前的旧数据，可以先比对一下再决定：");
        println!("    · 想留着看   → 暂存区就在 <盘符>:\\.diskdoctor-trash\\ops\\<操作ID>\\data\\");
        println!("    · 确认不需要 → diskdoctor trash purge --yes   （清掉这些旧数据）");
        // 继续占着空间这件事必须说，否则用户不知道空间没回来
        for t in &trashes {
            if let Ok((bytes, _)) = t.usage() {
                if bytes > 0 {
                    println!();
                    println!(
                        "  注意：暂存区当前仍占着 {} —— 文件在里面，直到 purge 才真正释放。",
                        human_size(bytes)
                    );
                }
            }
        }
    }

    let _ = restored_n;
    Ok(())
}

fn purge(id: Option<&str>, older_than: Option<u64>, dry_run: bool, yes: bool) -> Result<()> {
    use dd_exec::space::{available_bytes, SpaceDelta};

    let trashes = all_trashes();
    if trashes.is_empty() {
        println!("没有找到暂存区。");
        return Ok(());
    }

    // 先整体预览，拿到总量后再决定要不要二次确认
    let mut plan: Vec<(Trash, String, u64, usize)> = Vec::new();
    let mut already: Vec<(String, String)> = Vec::new();

    for t in &trashes {
        if let Some(want) = id {
            if !t.list(true)?.iter().any(|e| e.id == want) {
                continue;
            }
        }
        for o in t.purge(id, older_than, true)? {
            if o.already_cleared {
                already.push((
                    o.op_id.clone(),
                    format!("{}（已清理过，无内容可释放）", o.status_before.label()),
                ));
                continue;
            }
            plan.push((Trash::open(t.volume())?, o.op_id, o.freed, o.item_count));
        }
    }

    if !already.is_empty() {
        println!("以下批次无需处理：");
        for (op, why) in &already {
            println!("  · {op}  {why}");
        }
        println!();
    }

    if plan.is_empty() {
        if already.is_empty() {
            println!("没有匹配的待清除批次。");
            if let Some(d) = older_than {
                println!("（筛选条件：创建时间早于 {d} 天前）");
            }
        } else if plan.is_empty() && already.len() == 1 && id.is_some() {
            println!("该批次已经清理过了，本次没有释放任何空间。");
            println!("（重复执行 purge 是安全的，但不会再次释放空间 —— 空间只释放一次）");
        }
        return Ok(());
    }

    let total: u64 = plan.iter().map(|p| p.2).sum();
    let n: usize = plan.iter().map(|p| p.3).sum();

    println!("即将**彻底删除**以下内容，此操作不可撤销：");
    println!();
    for (_, op_id, freed, _) in &plan {
        println!("  {}  {}", op_id, human_size(*freed));
    }
    println!();
    println!("合计：{} / {} 项", human_size(total), n);

    if dry_run {
        println!();
        println!("以上为预览，未删除任何东西。");
        return Ok(());
    }

    if !yes {
        println!();
        println!("⚠ 此操作不可逆，删除后无法恢复。");
        println!("  确认请加 --yes 参数重新执行。");
        println!("  （先跑 `diskdoctor trash list` 核对一遍内容）");
        return Ok(());
    }

    // —— 记录清理前的可用空间 ——
    //
    // 这是让"释放了多少"从**声称**变成**实测**的关键。
    // 用户看到"已释放 2.13 GB"而磁盘没变，只会认为工具在骗人；
    // 实测数字即便不符，也至少是诚实的。
    let space_before: Vec<(String, Option<u64>)> = plan
        .iter()
        .map(|(t, _, _, _)| {
            let vol = t.volume().to_string_lossy().to_string();
            (vol.clone(), available_bytes(std::path::Path::new(&vol)))
        })
        .collect();

    let mut freed_total = 0u64;
    let mut count_total = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for (t, op_id, _, _) in &plan {
        for o in t.purge(Some(op_id), None, false)? {
            if o.already_cleared {
                skipped.push(o.op_id.clone());
                continue;
            }
            freed_total += o.freed;
            count_total += o.item_count;
        }
    }

    println!();
    if count_total == 0 {
        println!("⚠ 没有任何内容被删除。");
        if !skipped.is_empty() {
            println!("  这些批次在两次检查之间已被清理：{}", skipped.join(", "));
        }
        return Ok(());
    }

    println!("删除完成：{} 项，共 {}", count_total, human_size(freed_total));

    // —— 实测校验 ——
    //
    // 逐卷对比：每个卷只跟"自己那一批释放量"比，不能拿总量去比单个卷，
    // 否则跨卷清理时必然误报。
    println!();
    let mut verdicts: Vec<(String, DeltaVerdict, i64, u64)> = Vec::new();
    for (vol, before) in &space_before {
        let Some(b) = before else { continue };
        let Some(after) = available_bytes(std::path::Path::new(vol)) else {
            continue;
        };
        let delta = SpaceDelta {
            before: *b,
            after,
        };
        let claimed: u64 = plan
            .iter()
            .filter(|(t, _, _, _)| t.volume().to_string_lossy() == vol.as_str())
            .map(|(_, _, f, _)| *f)
            .sum();

        let d = delta.delta();
        let verdict = delta.verdict(claimed);

        if d >= 0 {
            println!(
                "{} 可用空间：{} → {}（实测释放 {}）",
                vol,
                human_size(*b),
                human_size(after),
                human_size(d as u64)
            );
        } else {
            println!(
                "{} 可用空间：{} → {}（反而少了 {}）",
                vol,
                human_size(*b),
                human_size(after),
                human_size((-d) as u64)
            );
        }
        verdicts.push((vol.clone(), verdict, d, claimed));
    }

    // 按判定给出针对性的解释 —— 不同成因的对策完全不同，不能笼统说"正常"
    for (vol, verdict, _d, claimed) in &verdicts {
        match verdict {
            DeltaVerdict::Freed => {}
            DeltaVerdict::NotReleased => {
                println!();
                println!("⚠ {vol} 上这次删除**没有真正归还空间**（声称删除 {}）。", human_size(*claimed));
                println!("  最常见的原因是：本机的删除被重定向了 —— 文件进了**回收站**，");
                println!("  仍然占着磁盘，直到清空回收站才会释放。");
                println!("  某些环境（安全软件、企业策略、代理运行时）会这样拦截删除。");
                println!();
                println!("  怎么确认与解决：");
                println!("    · 打开回收站看一眼，刚刚清理的内容是否在里面");
                println!("    · 确认不需要后清空回收站，空间才会真正回来");
                println!("    · 或跑 `diskdoctor {vol} --out report.md`，看「回收站」那一栏的体积");
            }
            DeltaVerdict::Shrunk => {
                println!();
                println!("说明：{vol} 的可用空间在本次操作期间**反而减少**了 ——");
                println!("  删除确实执行了，但同期有其他程序写入得更多（浏览器、更新服务、");
                println!("  编辑器索引、下载任务等）。这不代表清理失败。");
            }
            DeltaVerdict::Partial => {
                println!();
                println!("说明：{vol} 实测归还量少于声称的删除量。通常是这几个原因叠加：");
                println!("  · 部分内容被程序自动重建（缓存目录清掉后往往会立刻重新生成）");
                println!("  · 同期有其他程序在写盘");
                println!("  · 回收站若能容纳，部分内容可能被移入回收站而非直接删除");
            }
            DeltaVerdict::Unknown => {}
        }
    }

    Ok(())
}

fn fmt_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60
    )
}

// 复用 main 里的公共实现，避免两份日历算法各自演化导致不一致
use crate::civil_from_days_pub as civil_from_days;

/// 供 `clean` 复用：把一批路径按「可安全回收」清单送进暂存区。
pub fn stage_paths(
    reqs: &[dd_exec::StageRequest],
    note: &str,
    dry_run: bool,
) -> Result<()> {
    use dd_exec::TrashStatus;
    use std::collections::BTreeMap;

    if reqs.is_empty() {
        println!("没有待清理的项目。");
        return Ok(());
    }

    // 按卷分组：暂存区是每卷一个，所以要分开处理
    let mut by_vol: BTreeMap<String, Vec<dd_exec::StageRequest>> = BTreeMap::new();
    for r in reqs {
        let vol = dd_exec::guard::volume_root_of(&r.path)
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".into());
        by_vol.entry(vol).or_default().push(r.clone());
    }

    // **只统计真正移入的** —— 报告必须按事实说话。
    // 实测踩过：全批失败时仍打印"已移入 N 项"，是最危险的一类谎言。
    let mut done_n = 0usize;
    let mut done_bytes = 0u64;
    let mut guard_rejected: Vec<(String, String)> = Vec::new();
    let mut io_failed: Vec<(String, String)> = Vec::new();

    for (vol, items) in &by_vol {
        let t = Trash::open(std::path::Path::new(vol))
            .with_context(|| format!("无法打开 {vol} 的暂存区"))?;
        let out = t.stage(items, note, dry_run)?;

        let staged_n = out.entry.staged_count();
        let staged_bytes = out.entry.remaining();

        if dry_run {
            println!("[试运行] 卷 {vol}");
            for it in out.entry.items.iter().take(20) {
                println!(
                    "  {:>10}  {}{}",
                    human_size(it.size),
                    it.original_path,
                    if it.reason.is_empty() {
                        String::new()
                    } else {
                        format!("  ← {}", it.reason)
                    }
                );
            }
            if out.entry.items.len() > 20 {
                println!("  … 另有 {} 项", out.entry.items.len() - 20);
            }
            done_n += out.entry.items.len();
            done_bytes += out.entry.total_size;
        } else {
            match out.entry.status {
                TrashStatus::Cancelled => {
                    println!("卷 {vol}  操作 {}  **未能移入任何一项**", out.entry.id);
                }
                _ => {
                    println!(
                        "卷 {vol}  操作 {}  已移入 {} 项（{}）",
                        out.entry.id,
                        staged_n,
                        human_size(staged_bytes)
                    );
                }
            }
            done_n += staged_n;
            done_bytes += staged_bytes;
        }

        guard_rejected.extend(out.rejected.clone());

        // 移入失败的（被占用等）单独收集，附上原因与建议
        for it in out.entry.skipped_items() {
            io_failed.push((
                it.original_path.clone(),
                it.skipped_reason.clone().unwrap_or_default(),
            ));
        }
    }

    println!();
    if dry_run {
        println!("[试运行] 将移入暂存区 {} 项，共 {}", done_n, human_size(done_bytes));
        println!("未改动任何文件。去掉 --dry-run 即执行。");
    } else if done_n == 0 {
        println!("⚠ 没有任何文件被移入暂存区，你的文件都在原处。");
        println!("  原因见下方列表。");
    } else {
        println!("已移入暂存区 {} 项，共 {}", done_n, human_size(done_bytes));
        println!();
        println!("⚠ 现在空间**还没有真正释放** —— 文件只是从原位置移到了暂存区。");
        println!("  确认无误后执行：diskdoctor trash purge --older-than 7 --yes");
    }

    if !io_failed.is_empty() {
        println!();
        println!("以下 {} 项**未能移入**（它们仍在原处，未受影响）：", io_failed.len());
        for (p, why) in io_failed.iter().take(15) {
            println!("  ✗ {p}");
            println!("      {why}");
        }
        if io_failed.len() > 15 {
            println!("  … 另有 {} 项", io_failed.len() - 15);
        }
        let locked = io_failed
            .iter()
            .filter(|(_, w)| w.contains("占用") || w.contains("其他程序"))
            .count();
        if locked > 0 {
            println!();
            println!("  其中 {locked} 项是被运行中的程序占用了。这类文件无法在运行时移走 ——");
            println!("  可以稍后重试，或先关掉相关程序（浏览器、编辑器等）再跑一次。");
            println!("  其余项目不受影响，已经正常清理。");
        }
    }

    if !guard_rejected.is_empty() {
        println!();
        println!("以下 {} 项被安全护栏拒绝（未处理）：", guard_rejected.len());
        for (p, why) in &guard_rejected {
            println!("  ✗ {p}");
            println!("    {why}");
        }
    }

    Ok(())
}
