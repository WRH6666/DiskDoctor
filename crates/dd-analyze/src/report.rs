//! 报告层：把分析结果渲染成一份能直接看、能直接照做的体检报告。
//!
//! 报告的组织顺序刻意不是"按体积从大到小"，而是**按安全等级分档**：
//! 先告诉你"这些可以放心清"，再告诉你"这些要你确认"，最后才说
//! "这些不能删，但可以这样处理"。因为用户真正的卡点从来不是
//! "不知道哪里大"，而是"不知道该不该动"。

use crate::{Analysis, Finding};
use dd_core::{Safety, SpecialItem};
use serde::Serialize;

// 报告抬头要用卷信息，直接复用 dd-core 的模型，避免两处定义各说各话。
pub use dd_core::VolumeInfo;

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub generated_at: i64,
    pub backend: String,
    pub root: String,
    pub elapsed_ms: u64,
    pub volumes: Vec<VolumeInfo>,

    /// 扫描到的总字节数
    pub total_size: u64,
    pub total_alloc: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub skipped: u64,

    /// 可安全回收
    pub safe_size: u64,
    pub safe_items: u64,
    /// 需人工确认
    pub caution_size: u64,
    pub caution_items: u64,
    /// 用户数据
    pub keep_size: u64,
    /// 系统托管（可走官方命令回收的部分）
    pub system_size: u64,
    /// 规则库未覆盖
    pub unattributed: u64,

    pub top_safe: Vec<Finding>,
    pub top_caution: Vec<Finding>,
    pub specials: Vec<SpecialItem>,
    pub categories: Vec<crate::CategoryStat>,
    /// 规则库未覆盖的区域明细。数据盘场景下这是最该看的一节。
    pub unknowns: Vec<crate::UnknownRegion>,
}

const TOP_N: usize = 20;

impl HealthReport {
    pub fn build(a: &Analysis, volumes: Vec<VolumeInfo>) -> Self {
        let top_safe = a.top(Safety::Safe, TOP_N).into_iter().cloned().collect();
        let top_caution = a
            .top(Safety::Caution, TOP_N)
            .into_iter()
            .cloned()
            .collect();

        let safe = a.safeties.iter().find(|s| s.safety == Safety::Safe);
        let caution = a.safeties.iter().find(|s| s.safety == Safety::Caution);
        let keep = a.safeties.iter().find(|s| s.safety == Safety::Keep);
        let sysman = a.safeties.iter().find(|s| s.safety == Safety::SystemManaged);

        Self {
            generated_at: now_unix(),
            backend: a.backend.clone(),
            root: a.snapshot.root_path.clone(),
            elapsed_ms: a.snapshot.elapsed_ms,
            volumes,
            total_size: a.snapshot.total_size(),
            total_alloc: a.snapshot.total_alloc(),
            file_count: a.snapshot.file_count(),
            dir_count: a.snapshot.dir_count(),
            skipped: a.snapshot.skipped_count,
            safe_size: safe.map(|s| s.size).unwrap_or(0),
            safe_items: safe.map(|s| s.item_count).unwrap_or(0),
            caution_size: caution.map(|s| s.size).unwrap_or(0),
            caution_items: caution.map(|s| s.item_count).unwrap_or(0),
            keep_size: keep.map(|s| s.size).unwrap_or(0),
            system_size: sysman.map(|s| s.size).unwrap_or(0),
            unattributed: a.unattributed,
            top_safe,
            top_caution,
            specials: a.specials().to_vec(),
            categories: a.categories.clone(),
            unknowns: a.unknowns.clone(),
        }
    }

    /// 本轮"确定可以安全回收"的量（不含系统托管部分）。
    pub fn confident_reclaim(&self) -> u64 {
        self.safe_size
    }

    /// 系统级"隐形占用"的已采集总量。
    ///
    /// 这一部分**不计入** `safe_size` / `system_size`：它们要么读不到
    /// （需要管理员权限），要么根本不在常规目录树里（卷影副本、页面文件），
    /// 混进归因结果会算重。报告里单列一节。
    pub fn specials_total(&self) -> u64 {
        self.specials
            .iter()
            .filter(|s| s.ok && s.size > 0)
            .map(|s| s.size)
            .sum()
    }

    /// 含系统级隐藏在內的潜在可回收上限（乐观口径，需配合官方命令）。
    pub fn potential_reclaim(&self) -> u64 {
        self.safe_size + self.system_size + self.specials_total()
    }

    pub fn to_markdown(&self) -> String {
        let mut s = String::new();

        s.push_str("# 磁盘体检报告\n\n");
        s.push_str(&format!(
            "> 扫描路径 `{}` ｜ 后端 `{}` ｜ 耗时 {:.1}s ｜ 生成于 {}\n\n",
            self.root,
            self.backend,
            self.elapsed_ms as f64 / 1000.0,
            fmt_time(self.generated_at)
        ));

        // —— 卷概况 ——
        if !self.volumes.is_empty() {
            s.push_str("## 卷概况\n\n");
            s.push_str("| 卷 | 总量 | 已用 | 可用 | 使用率 |\n|---|---|---|---|---|\n");
            for v in &self.volumes {
                s.push_str(&format!(
                    "| `{}` | {} | {} | **{}** | {:.0}% |\n",
                    v.root,
                    human_size(v.total),
                    human_size(v.used()),
                    human_size(v.free),
                    v.used_ratio() * 100.0
                ));
            }
            s.push('\n');
        }

        // —— 一句话结论 ——
        s.push_str("## 结论\n\n");
        s.push_str(&format!(
            "- 本次扫到 **{}** 个文件、**{}** 个目录，合计 **{}**\n",
            num(self.file_count),
            num(self.dir_count),
            human_size(self.total_size)
        ));

        // 扫到的比卷已用还多，一定要解释，否则用户会以为数字算错了。
        // 根因是硬链接（WinSxS 是硬链接仓库）被重复计算 —— 遍历后端无法
        // 廉价地拿到硬链接数，这是已知限制，MFT 后端接入后消除。
        if let Some(used) = scanned_volume_used(&self.root, &self.volumes) {
            if self.total_size > used && used > 0 {
                let over = self.total_size - used;
                s.push_str(&format!(
                    "\n> **读到这里可能觉得对不上账**：扫到的比这个卷的已用空间（{}）还多 **{}**。\n\
                     > 原因通常是 **NTFS 硬链接被重复计算** —— 最典型的是 `C:\\Windows\\WinSxS`，\
                     它是硬链接仓库，同一份数据在多个组件下各算了一次。\
                     目前使用的遍历后端无法廉价地拿到每个文件的硬链接数（要为每个文件开句柄），\
                     这一点已在 `dd-scanner/src/walk.rs` 的 `links_of()` 里写明，\
                     MFT 直读后端接入后会按文件引用号去重，届时数字会与资源管理器一致。\n\n",
                    human_size(used),
                    human_size(over)
                ));
            }
        }
        s.push_str(&format!(
            "- **可安全回收：{}** —— 删了能自动重建或重新下载，共 {} 处\n",
            human_size(self.safe_size),
            num(self.safe_items)
        ));
        if self.system_size > 0 {
            s.push_str(&format!(
                "- 系统托管可回收：**{}** —— 不能直接删，需要执行官方命令，见下方「系统级占用」\n",
                human_size(self.system_size)
            ));
        }
        let sp = self.specials_total();
        if sp > 0 {
            s.push_str(&format!(
                "- 系统级隐形占用：**{}** —— 休眠文件 / 页面文件 / 卷影副本，\
                 普通扫描器根本看不到或不敢报的部分，见下方专节\n",
                human_size(sp)
            ));
        }
        if self.caution_size > 0 {
            s.push_str(&format!(
                "- 需要你确认：**{}** —— 里面可能混着你要留的东西，共 {} 处\n",
                human_size(self.caution_size),
                num(self.caution_items)
            ));
        }
        if self.keep_size > 0 {
            s.push_str(&format!(
                "- 属于个人数据：**{}** —— 本工具不会主动清理\n",
                human_size(self.keep_size)
            ));
        }
        if self.unattributed > 0 {
            s.push_str(&format!(
                "- 规则库未覆盖：**{}** —— 见文末「未识别」，这部分需要人工判断\n",
                human_size(self.unattributed)
            ));
        }
        s.push('\n');

        // —— 系统级隐形占用 ——
        if !self.specials.is_empty() {
            s.push_str("## 系统级占用（普通扫描器看不到的部分）\n\n");
            s.push_str("> 以下按**整台机器**统计，跨所有卷 —— 因为它通常就不在你要扫的那个目录里。\n\n");
            s.push_str("| 项目 | 所在卷 | 体积 | 处理方式 |\n|---|---|---|---|\n");
            for it in &self.specials {
                let size = if it.ok {
                    human_size(it.size)
                } else {
                    "未能读取".to_string()
                };
                s.push_str(&format!(
                    "| {} | `{}` | {} | {} |\n",
                    it.name,
                    volume_of(&it.path),
                    size,
                    it.action_text()
                ));
            }
            s.push('\n');
            for it in &self.specials {
                s.push_str(&format!("### {}\n\n", it.name));
                s.push_str(&format!("- 路径：`{}`\n", it.path));
                if it.ok {
                    s.push_str(&format!("- 体积：**{}**\n", human_size(it.size)));
                } else {
                    s.push_str("- 体积：**未能读取**\n");
                }
                s.push_str(&format!("- 处理方式：{}\n", it.action_text()));
                s.push_str(&format!("- 为什么：{}\n", it.rationale));
                s.push_str(&format!("- 恢复：{}\n", it.recovery));
                s.push_str(&format!("- 数据来源：{}\n\n", it.source));
            }
        }

        // —— 可安全回收清单 ——
        s.push_str(&format!(
            "## 可以放心清理的（按体积降序，前 {} 项）\n\n",
            self.top_safe.len()
        ));
        if self.top_safe.is_empty() {
            s.push_str("本次没有发现可安全回收的项目。\n\n");
        } else {
            s.push_str(&finding_table(&self.top_safe));
            s.push_str("\n");
            for f in self.top_safe.iter().take(10) {
                s.push_str(&format!("### {} — {}\n\n", human_size(f.size), f.rule_name));
                s.push_str(&format!("- 路径：`{}`\n", f.path));
                s.push_str(&format!(
                    "- 内容：{} 个文件，{}\n",
                    num(f.file_count),
                    f.category_label
                ));
                s.push_str(&format!("- 为什么能删：{}\n", f.why));
                s.push_str(&format!("- 怎么删：{}\n", f.action_hint()));
                s.push_str(&format!("- 删了怎么恢复：{}\n\n", f.recovery));
            }
        }

        // —— 需确认 ——
        s.push_str(&format!(
            "## 需要你确认的（按体积降序，前 {} 项）\n\n",
            self.top_caution.len()
        ));
        if self.top_caution.is_empty() {
            s.push_str("无。\n\n");
        } else {
            s.push_str(&finding_table(&self.top_caution));
            s.push('\n');
        }

        // —— 未识别明细 ——
        if !self.unknowns.is_empty() {
            s.push_str(&format!(
                "## 未识别 TOP {}（规则库待补充）\n\n",
                self.unknowns.len()
            ));
            s.push_str(
                "这些路径规则库没有覆盖，工具**无法判断能不能删**。\n\
                 它们不是不重要 —— 恰恰相反，这是最需要你自己去确认的部分。\n\n",
            );
            s.push_str("| 体积 | 位置 | 文件数 |\n|---|---|---|\n");
            for u in &self.unknowns {
                s.push_str(&format!(
                    "| **{}** | `{}` | {} |\n",
                    human_size(u.size),
                    shorten(&u.path, 80),
                    num(u.file_count)
                ));
            }
            s.push('\n');
            s.push_str(
                "> 如果你确认某个目录属于「个人数据、工具不该动」，\
                 可以在 `crates/dd-rules/rules/rules.yaml` 里加一条规则把它标成 \
                 `keep`；下次扫描它就不会再出现在这里。\n\n",
            );
        }

        // —— 类别汇总 ——
        s.push_str("## 按类别汇总\n\n");
        s.push_str("| 类别 | 体积 | 文件数 | 处数 | 口径 |\n|---|---|---|---|---|\n");
        for c in &self.categories {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                c.label,
                human_size(c.size),
                num(c.file_count),
                num(c.item_count),
                c.worst_safety.label()
            ));
        }
        s.push('\n');

        if self.skipped > 0 {
            s.push_str(&format!(
                "> 提示：有 {} 处路径因权限不足被跳过。以管理员身份重新运行可以获得完整数据。\n\n",
                num(self.skipped)
            ));
        }

        s.push_str("---\n\n");
        s.push_str(
            "本报告为**只读分析**，没有删除或修改任何文件。\n\
             所有路径都可以复制出来自行核对。\n",
        );

        s
    }
}

fn finding_table(items: &[Finding]) -> String {
    let mut s = String::new();
    s.push_str("| 体积 | 项目 | 位置 | 文件数 | 处置 |\n|---|---|---|---|---|\n");
    for f in items {
        s.push_str(&format!(
            "| **{}** | {} | `{}` | {} | {} |\n",
            human_size(f.size),
            f.rule_name,
            shorten(&f.path, 64),
            num(f.file_count),
            f.safety_label
        ));
    }
    s
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if b < KB * KB {
        return format!("{:.1} KB", b / KB);
    }
    if b < KB * KB * KB {
        return format!("{:.1} MB", b / (KB * KB));
    }
    if b < KB * KB * KB * KB {
        return format!("{:.2} GB", b / (KB * KB * KB));
    }
    format!("{:.2} TB", b / (KB * KB * KB * KB))
}

fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 扫描根对应的那个卷的「已用空间」。找不到返回 None。
fn scanned_volume_used(root: &str, volumes: &[VolumeInfo]) -> Option<u64> {
    let b = root.as_bytes();
    if b.len() < 2 || b[1] != b':' {
        return None;
    }
    let letter = (b[0] as char).to_ascii_uppercase();
    volumes
        .iter()
        .find(|v| v.root.to_ascii_uppercase().starts_with(letter))
        .map(|v| v.used())
}

fn shorten(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let head: String = chars[..max / 2].iter().collect();
    let tail: String = chars[chars.len() - max / 2..].iter().collect();
    format!("{head}…{tail}")
}

/// 从路径里取出卷根（`C:\` / `D:\`）。用于让报告说清某一项在哪个盘上。
fn volume_of(path: &str) -> String {
    let b = path.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        format!("{}:\\", (b[0] as char).to_ascii_uppercase())
    } else {
        "—".to_string()
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 Unix 秒格式化成 `YYYY-MM-DD HH:MM`（本地时区近似，用 UTC 偏移粗算）。
fn fmt_time(unix: i64) -> String {
    // 不引 chrono：这里只需要一个可读的绝对时间，用简单算法做民用日历换算。
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Howard Hinnant 的 days_from_civil 逆运算。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
