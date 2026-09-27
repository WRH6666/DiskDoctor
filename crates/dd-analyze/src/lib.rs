//! 分析层：把"一堆目录"变成"一份能下决定的清单"。
//!
//! 三件事：
//! 1. **归因** —— 每条路径挂上规则库里的解释：这是什么、能不能删、怎么删。
//! 2. **聚合成区域** —— 不做"每个文件一条"，而是找出**归因区域的根**：
//!    比如 `…/proj/node_modules` 下面几万个文件只汇报成一条。这是普通工具
//!    最缺的一步，用户要的是"这几坨东西"，不是"这 30 万个文件"。
//! 3. **分级汇总** —— 按安全等级算清"真正能安全回收多少"，
//!    并把这部分和"必须走官方命令的""属于你个人数据的"严格分开。

pub mod report;

pub use report::{human_size, HealthReport};

use dd_core::{Action, Category, Safety, Snapshot, SpecialItem, ROOT_PARENT};
use dd_rules::Ruleset;
use serde::Serialize;

/// 未命中任何规则。
pub const UNATTRIBUTED: u32 = u32::MAX;

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStat {
    pub category: Category,
    pub label: String,
    pub size: u64,
    pub alloc: u64,
    pub file_count: u64,
    /// 该类别下**最保守**的安全等级。
    ///
    /// 刻意取最严的一档而不是最宽的一档：一个类别里只要混进一条
    /// 「需确认」，整类的口径就得是「需确认」。在这里乐观会让用户
    /// 以为可以整类清掉，而那正是最容易出事的地方。
    pub worst_safety: Safety,
    pub item_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SafetyStat {
    pub safety: Safety,
    pub label: String,
    pub size: u64,
    pub file_count: u64,
    pub item_count: u64,
}

/// 一条"可下手的对象"：归因区域的根。
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub alloc: u64,
    pub is_dir: bool,
    /// 该区域内的文件数
    pub file_count: u64,
    pub category: Category,
    pub category_label: String,
    pub safety: Safety,
    pub safety_label: String,
    /// 可删评分 0–100
    pub score: u8,
    pub rule_id: String,
    pub rule_name: String,
    pub why: String,
    pub recovery: String,
    pub action: Action,
    /// 归因是"从上级目录继承来的"还是"这条路径自己命中的"
    pub inherited: bool,
    /// 最后修改时间（Unix 秒），0 表示未知
    pub mtime: i64,
}

impl Finding {
    pub fn action_hint(&self) -> String {
        match &self.action {
            Action::Delete => "清理（移入暂存区，可撤销）".into(),
            Action::Command { cmd } => format!("执行官方命令：{cmd}"),
            Action::Compact => "压缩虚拟磁盘（不丢数据）".into(),
            Action::Review => "需要人工确认".into(),
            Action::None => "不做处理".into(),
        }
    }
}

/// 未归因区域。对"数据盘"场景来说这是报告里最重要的一节：
/// 规则库主要覆盖系统盘和开发缓存，而用户的 D 盘往往是
/// 学习资料、游戏、视频、安装包、工程文件 —— 这些要么该识别成
/// 「用户数据」（明确告诉用户工具不会动），要么该补规则，
/// 绝不该默默淹没成一句"未识别 XX GB"。
#[derive(Debug, Clone, Serialize)]
pub struct UnknownRegion {
    pub path: String,
    pub size: u64,
    pub file_count: u64,
}

pub struct Analysis {
    pub snapshot: Snapshot,
    pub rules: Ruleset,
    /// 每个条目命中的规则下标，`UNATTRIBUTED` 表示未命中
    pub attrib: Vec<u32>,
    pub findings: Vec<Finding>,
    pub categories: Vec<CategoryStat>,
    pub safeties: Vec<SafetyStat>,
    /// 未归因（规则库没覆盖）的总字节数
    pub unattributed: u64,
    /// 未归因区域的明细，按体积降序
    pub unknowns: Vec<UnknownRegion>,
    pub backend: String,
}

impl Analysis {
    pub fn run(mut snapshot: Snapshot, rules: Ruleset, backend: impl Into<String>) -> Self {
        if snapshot.root_prefix.is_empty() {
            // 防御：调用方可能直接构造了 Snapshot 而没走 finalize
            snapshot.finalize();
        }

        let attrib = attribute_all(&snapshot, &rules);
        let (findings, attributed) = analyze_regions(&snapshot, &rules, &attrib);
        let (categories, safeties) = aggregate(&findings);
        let unknowns = collect_unknowns(&snapshot, &attrib);

        let total = snapshot.total_size();
        let unattributed = total.saturating_sub(attributed);

        Self {
            snapshot,
            rules,
            attrib,
            findings,
            categories,
            safeties,
            unattributed,
            unknowns,
            backend: backend.into(),
        }
    }

    /// 按安全等级取"可回收字节数"。
    pub fn reclaimable(&self, safety: Safety) -> u64 {
        self.safeties
            .iter()
            .filter(|s| s.safety == safety)
            .map(|s| s.size)
            .sum()
    }

    pub fn specials(&self) -> &[SpecialItem] {
        &self.snapshot.specials
    }

    /// 取指定安全等级下最大的若干条，按体积降序。
    pub fn top(&self, safety: Safety, n: usize) -> Vec<&Finding> {
        let mut v: Vec<&Finding> = self.findings.iter().filter(|f| f.safety == safety).collect();
        v.sort_by(|a, b| b.size.cmp(&a.size));
        v.truncate(n);
        v
    }

    pub fn build_report(&self, volumes: Vec<dd_core::VolumeInfo>) -> HealthReport {
        report::HealthReport::build(self, volumes)
    }
}

/// 给每条路径挂规则。
///
/// # 继承语义（关键）
///
/// 每条路径有「自己的规则」和「从父目录继承」两个来源，取 **priority 更高**
/// 的那个；自己没命中就继承。
///
/// 目录也**必须**继承，这一点最初漏掉了，后果很严重：`path-suffix` 规则只
/// 能命中恰好等于该路径的那个目录，于是 `C:\Windows\WinSxS` 本身被判为
/// 「组件存储」，而它下面的 `Manifests`、`FileRepository` 全部变成「未识别」——
/// 整棵子树被拆散，未识别量虚高、系统托管量严重低估。
///
/// 有了继承，`path-suffix` 才真正是"标记一个区域的根，其下全部归属它"。
///
/// 用 priority 而不是"自己命中就赢"，是为了让 `windows.old\windows` 这
/// 种情况仍然归到 `windows.old`（95）而不是被宽泛的 `windows`（27）抢走。
///
/// 单趟即可：父节点下标恒小于子节点下标，处理到 i 时父节点的归因已经算好了。
fn attribute_all(snap: &Snapshot, rules: &Ruleset) -> Vec<u32> {
    let n = snap.entries.len();
    let mut attrib = vec![UNATTRIBUTED; n];
    let needs_path = rules.needs_path_context();

    // —— 扫描根自身的归因，必须单独算出来 ——
    //
    // 这是修一个真实缺陷：快照从扫描根开始，**根之上的路径不在里面**，
    // 所以上层的语境无法参与继承。
    //
    // 具体表现：扫 `C:\Users\me\.workbuddy\proj` 时，真正决定性质的是
    // 中间的 `.workbuddy`（规则 workbuddy-data，禁止触碰）。但快照里
    // 没有 `.workbuddy` 这个条目，于是 `proj\node_modules` 只按自己的
    // 名字判成了「可安全回收」；而同一条路径用 `explain`（走完整绝对路径）
    // 却判成「禁止触碰」。
    //
    // **同一路径两个答案**直接摧毁用户对工具的信任 —— 界面说能删、
    // 执行时被拒绝，用户就不知道该信哪个。所以必须在归因源头补上这一步：
    // 用完整路径算出根的性质，作为所有顶层条目的继承起点。
    let root_attr = rules
        .attribute_path(&snap.root_path, true)
        .map(|i| i as u32);

    for i in 0..n {
        let e = &snap.entries[i];
        let parent_attr = if e.parent != ROOT_PARENT {
            attrib[e.parent as usize]
        } else {
            UNATTRIBUTED
        };

        // 路径信息对**文件和目录都要**准备：语境约束
        // （比如 `node_modules` 在 IDE 扩展目录里不适用）依赖它，
        // 而语境约束也可能挂在文件规则上。
        let path_lower = if needs_path {
            Some(snap.lower_dir_path(i as u32))
        } else {
            None
        };

        let own = if e.is_dir {
            rules.attribute_dir(&e.name, path_lower.as_deref())
        } else {
            rules.attribute_file(&e.name, path_lower.as_deref())
        };

        attrib[i] = if i == 0 {
            // 扫描根：优先采用含上层语境的完整归因
            root_attr.or_else(|| own.map(|o| o as u32)).unwrap_or(UNATTRIBUTED)
        } else {
            match (own, parent_attr) {
                (Some(o), UNATTRIBUTED) => o as u32,
                // 自己没命中 → 继承父目录
                (None, p) => p,
                (Some(o), p) => {
                    let po = rules.rule(o).priority;
                    let pp = rules.rule(p as usize).priority;
                    // 平手时用这条路径自己的规则
                    if po >= pp {
                        o as u32
                    } else {
                        p
                    }
                }
            }
        };
    }

    attrib
}

/// 「没有归属区域」的哨兵值。用条目下标之外的值，避免和下标 0 混淆。
const NO_OWNER: u32 = u32::MAX;

/// 归并成互不重叠的"区域"，并算出各自的体积。
///
/// 拆成两步：
/// 1. `ex`（独占大小）：目录的 size 减去所有直接子项。目录自身不占字节，
///    所以 `ex[dir]` 恒为 0，只有文件有独占大小；全部 `ex` 之和 = 总量。
/// 2. `owner`：覆盖每个条目的那个区域根。
///
/// 然后每个区域根只累加「`owner` 指向它」的条目的 `ex`。这样区域之间天然
/// 不重叠，所有区域根体积之和恰好等于已归因总量。
fn analyze_regions(snap: &Snapshot, rules: &Ruleset, attrib: &[u32]) -> (Vec<Finding>, u64) {
    let n = snap.entries.len();

    let mut ex: Vec<u64> = snap.entries.iter().map(|e| e.size).collect();
    for i in 1..n {
        let p = snap.entries[i].parent;
        if p != ROOT_PARENT {
            ex[p as usize] = ex[p as usize].saturating_sub(snap.entries[i].size);
        }
    }

    let mut owner: Vec<u32> = vec![NO_OWNER; n];
    for i in 0..n {
        if attrib[i] == UNATTRIBUTED {
            continue;
        }
        let p = snap.entries[i].parent;
        if p != ROOT_PARENT && attrib[p as usize] == attrib[i] {
            owner[i] = owner[p as usize];
        } else {
            // 归因与父节点不同 → 它是这一片区域的根
            owner[i] = i as u32;
        }
    }

    let mut sizes = vec![0u64; n];
    let mut counts = vec![0u64; n];
    for i in 0..n {
        let o = owner[i];
        if o == NO_OWNER {
            continue;
        }
        sizes[o as usize] += ex[i];
        if !snap.entries[i].is_dir {
            counts[o as usize] += 1;
        }
    }

    let mut out = Vec::new();
    for i in 0..n {
        if owner[i] != i as u32 || sizes[i] == 0 {
            continue;
        }
        let rule = rules.rule(attrib[i] as usize);
        let e = &snap.entries[i];
        out.push(Finding {
            path: snap.abs_path(i as u32),
            name: e.name.to_string(),
            size: sizes[i],
            alloc: e.alloc,
            is_dir: e.is_dir,
            file_count: counts[i],
            category: rule.category,
            category_label: rule.category.label().to_string(),
            safety: rule.safety,
            safety_label: rule.safety.label().to_string(),
            score: rule.safety.score(),
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            why: rule.why.clone(),
            recovery: rule.recovery.clone(),
            action: rule.action.clone(),
            inherited: false,
            mtime: e.mtime,
        });
    }

    let attributed: u64 = sizes.iter().sum();
    (out, attributed)
}

/// 收集未归因区域的明细，按体积降序。
///
/// 刻意**不把扫描根的直接子项并成一大坨** —— 那样只会得到一句
/// 「未识别 525 GB」，对用户和补规则都毫无帮助。根的直接子项各自成区，
/// 于是报告能列出"这个盘上到底是哪几坨没被识别"。
fn collect_unknowns(snap: &Snapshot, attrib: &[u32]) -> Vec<UnknownRegion> {
    let n = snap.entries.len();

    let mut ex: Vec<u64> = snap.entries.iter().map(|e| e.size).collect();
    for i in 1..n {
        let p = snap.entries[i].parent;
        if p != ROOT_PARENT {
            ex[p as usize] = ex[p as usize].saturating_sub(snap.entries[i].size);
        }
    }

    let mut owner = vec![NO_OWNER; n];
    for i in 0..n {
        if attrib[i] != UNATTRIBUTED {
            continue;
        }
        let p = snap.entries[i].parent;
        // 父节点也是未归因且不是扫描根 → 归给同一个区域根
        if p != ROOT_PARENT && p != 0 && attrib[p as usize] == UNATTRIBUTED {
            owner[i] = owner[p as usize];
        } else {
            owner[i] = i as u32;
        }
    }

    let mut sizes = vec![0u64; n];
    let mut counts = vec![0u64; n];
    for i in 0..n {
        let o = owner[i];
        if o == NO_OWNER {
            continue;
        }
        sizes[o as usize] += ex[i];
        if !snap.entries[i].is_dir {
            counts[o as usize] += 1;
        }
    }

    let mut out: Vec<UnknownRegion> = (0..n)
        .filter(|&i| owner[i] == i as u32 && sizes[i] > 0)
        .map(|i| UnknownRegion {
            path: snap.abs_path(i as u32),
            size: sizes[i],
            file_count: counts[i],
        })
        .collect();

    out.sort_by(|a, b| b.size.cmp(&a.size));
    out.truncate(20);
    out
}

fn aggregate(findings: &[Finding]) -> (Vec<CategoryStat>, Vec<SafetyStat>) {
    let mut cats: Vec<CategoryStat> = Vec::new();
    let mut safes: Vec<SafetyStat> = Vec::new();

    for f in findings {
        match cats.iter_mut().find(|c| c.category == f.category) {
            Some(c) => {
                c.size += f.size;
                c.alloc += f.alloc;
                c.file_count += f.file_count;
                c.item_count += 1;
                // Safety 的声明顺序是 Safe < Caution < … < Protected，
                // 所以取较大值 = 取更保守的一档。
                if f.safety > c.worst_safety {
                    c.worst_safety = f.safety;
                }
            }
            None => cats.push(CategoryStat {
                category: f.category,
                label: f.category.label().to_string(),
                size: f.size,
                alloc: f.alloc,
                file_count: f.file_count,
                worst_safety: f.safety,
                item_count: 1,
            }),
        }

        match safes.iter_mut().find(|s| s.safety == f.safety) {
            Some(s) => {
                s.size += f.size;
                s.file_count += f.file_count;
                s.item_count += 1;
            }
            None => safes.push(SafetyStat {
                safety: f.safety,
                label: f.safety.label().to_string(),
                size: f.size,
                file_count: f.file_count,
                item_count: 1,
            }),
        }
    }

    cats.sort_by(|a, b| b.size.cmp(&a.size));
    safes.sort_by(|a, b| b.size.cmp(&a.size));
    (cats, safes)
}
