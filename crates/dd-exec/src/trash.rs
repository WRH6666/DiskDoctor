//! 暂存区：把「删除」变成「可撤销的移动」。
//!
//! # 核心思路
//!
//! 删除是不可逆的，但**移动不是**。同一个卷内把一个文件移到另一个目录，
//! 底层只是改了个名字（NTFS 的目录项），数据块一个字节都不动 ——
//! 所以几 GB 的文件也是毫秒级完成，不像复制那样要等。
//!
//! 于是：清理 = 移进暂存区。后悔了就移回来。
//!
//! # 为什么每个卷放一个暂存区
//!
//! 这是让上面那句话成立的前提。如果暂存区固定在 C 盘，那么清理 D 盘文件
//! 就必须**跨卷复制** —— 想删 20 GB 得先有 20 GB 的空闲空间，还有可能
//! 中途失败留下半份拷贝。把暂存区放在被清理文件**所在的那个卷**上，
//! 所有操作就都退化成同卷 rename，永远不需要额外空间、永远瞬时完成。
//!
//! 代价是暂存区分散在各盘。用 `diskdoctor trash list` 汇总查看即可。
//!
//! # 空间什么时候真正释放
//!
//! **移进暂存区不会释放空间。** 这是安全换来的代价：文件还在盘上，
//! 只是换了位置。必须执行 `purge` 才真正回收。
//!
//! 所以流程是：`stage`（可悔改）→ 观察几天 → `purge`（真正回收）。
//! 想省事可以直接 `purge --older-than 7` 定期清理超过 7 天的。
//!
//! # 断电安全
//!
//! 每次操作目录下都有一份 `manifest.json`。执行顺序是：
//!
//! 1. 写 manifest，状态 `pending`，**fsync 落盘**
//! 2. 逐项 rename
//! 3. 重写 manifest，状态 `staged`
//!
//! 如果在第 2 步断电，重启后会看到一个 `pending` 的 manifest ——
//! 里面记着完整计划，所以能判断出「哪些已经移走了、哪些还没有」，
//! 不会出现"文件在暂存区但没人知道它从哪来"的孤儿。

use crate::guard::{self, GuardError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    #[error("安全护栏拒绝: {0}")]
    Guard(#[from] GuardError),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("找不到操作记录: {0}")]
    NoSuchOp(String),

    #[error("无法判断 {0} 所在的卷")]
    NoVolume(String),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("暂存区状态异常: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, TrashError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrashStatus {
    /// 已写入计划但尚未执行完 —— 只会在断电/崩溃后出现
    Pending,
    /// 已移入暂存区，可恢复
    Staged,
    /// 部分已恢复（恢复过程中有冲突或失败）
    PartiallyRestored,
    /// 已全部恢复
    Restored,
    /// 已真正删除，不可恢复
    Purged,
    /// 一项都没能移入（例如全部被占用），暂存区里没有内容。
    ///
    /// 刻意与 `Restored` 区分：`Restored` 表示"曾经移入、后来恢复"，
    /// 而这个是"从未移入"。混用会让报告说不出真话 —— 实测踩过：
    /// 一次全批失败回滚后，CLI 仍打印"已移入 163 项"。
    Cancelled,
}

impl TrashStatus {
    pub fn label(&self) -> &'static str {
        match self {
            TrashStatus::Pending => "执行中断（需处理）",
            TrashStatus::Staged => "可恢复",
            TrashStatus::PartiallyRestored => "部分已恢复",
            TrashStatus::Restored => "已恢复",
            TrashStatus::Purged => "已彻底删除",
            TrashStatus::Cancelled => "已取消（未能移入）",
        }
    }

    /// 这一批当前是否还占着磁盘空间。
    pub fn occupies_space(&self) -> bool {
        matches!(self, TrashStatus::Staged | TrashStatus::PartiallyRestored)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashItem {
    /// 在暂存区内的序号，从 1 开始
    pub seq: u32,
    /// 原始绝对路径
    pub original_path: String,
    /// 在暂存区里的名字（`0001_原名`）
    pub stored_name: String,
    pub size: u64,
    pub is_dir: bool,
    /// 为什么清理它（来自规则库的中文名）
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub rule_id: String,
    /// 是否已恢复
    #[serde(default)]
    pub restored: bool,
    /// 若这一项**未能移入**（被占用、权限不足等），这里记录原因。
    ///
    /// 存在这个字段，才能既做到"尽力而为"（一项失败不影响其余），
    /// 又不丢信息（用户能确切知道哪些没清成、为什么）。
    #[serde(default)]
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrashEntry {
    pub id: String,
    pub created_at: i64,
    /// 暂存区所在的卷根，如 `D:\`
    pub volume: String,
    pub status: TrashStatus,
    pub items: Vec<TrashItem>,
    pub total_size: u64,
    /// 用户给这一批起的名字，便于识别
    #[serde(default)]
    pub note: String,
    /// 真正删除的时间
    #[serde(default)]
    pub purged_at: i64,
}

impl TrashEntry {
    /// 这一批当前是否还占着磁盘空间。
    ///
    /// 由状态决定，而不是看条目上的标记 —— 这样才不会出现
    /// "已 purge 的批次仍被算作占用 2.13 GB" 这种错误。
    pub fn holds_space(&self) -> bool {
        self.status.occupies_space()
    }

    /// 当前真正躺在暂存区里的条目（跳过失败的和已恢复的）。
    ///
    /// **必须先看批次状态**：已经 purge 掉的批次，其条目上的
    /// `skipped_reason` / `restored` 标记都还是原样，只按标记过滤
    /// 会把已删除的内容重新算成"占用中" —— 实测踩过：
    /// 对同一批次重复 purge，第二次仍报告"已释放 2.13 GB（151 项）"。
    pub fn staged_items(&self) -> impl Iterator<Item = &TrashItem> {
        let holds = self.holds_space();
        self.items
            .iter()
            .filter(move |i| holds && i.skipped_reason.is_none() && !i.restored)
    }

    /// 未能移入的条目。
    pub fn skipped_items(&self) -> impl Iterator<Item = &TrashItem> {
        self.items.iter().filter(|i| i.skipped_reason.is_some())
    }

    /// 真正移入暂存区的字节数 —— **不含**未能移入的那些，
    /// 也不含已被清除的批次。
    pub fn remaining(&self) -> u64 {
        self.staged_items().map(|i| i.size).sum()
    }

    /// 实际移入的条目数。
    pub fn staged_count(&self) -> usize {
        self.staged_items().count()
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

// ------------------------------------------------------------------ 请求

#[derive(Debug, Clone)]
pub struct StageRequest {
    pub path: PathBuf,
    /// 为什么清理（用于日志与报告）
    pub reason: String,
    pub rule_id: String,
    /// 已知大小；传 0 表示让暂存区自己算
    pub size: u64,
}

impl StageRequest {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            reason: String::new(),
            rule_id: String::new(),
            size: 0,
        }
    }

    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = r.into();
        self
    }

    pub fn rule(mut self, id: impl Into<String>) -> Self {
        self.rule_id = id.into();
        self
    }

    pub fn size(mut self, s: u64) -> Self {
        self.size = s;
        self
    }
}

#[derive(Debug)]
pub struct StageOutcome {
    pub entry: TrashEntry,
    /// 因护栏或 IO 问题被跳过的 (路径, 原因)
    pub rejected: Vec<(String, String)>,
    pub dry_run: bool,
}

#[derive(Debug)]
pub struct RestoreOutcome {
    pub op_id: String,
    pub restored: usize,
    pub skipped: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct PurgeOutcome {
    pub op_id: String,
    pub freed: u64,
    pub item_count: usize,
    pub dry_run: bool,
    /// 这一批**早已被清理过**（或从未占用空间），本次什么也没做。
    ///
    /// 存在这个字段是为了让报告说真话：对同一批次重复 purge 时，
    /// 不能再次声称"已释放 X GB"。
    pub already_cleared: bool,
    /// 该批次在此次调用前的状态，用于解释为何没清理
    pub status_before: TrashStatus,
}

// ------------------------------------------------------------------ 暂存区

pub struct Trash {
    /// `<卷根>\.diskdoctor-trash`
    root: PathBuf,
    volume: PathBuf,
}

impl Trash {
    /// 定位某个卷的暂存区。不创建目录。
    ///
    /// 若设置了环境变量 `DISKDOCTOR_TRASH_DIR`，则所有卷都用这个目录 ——
    /// 这是给测试用的：否则 `cargo test` 会在**真实用户的暂存区**里留下
    /// 一堆测试条目，`trash list` 时冒出来，非常糟糕。生产使用不要设它。
    pub fn open(volume_root: &Path) -> Result<Self> {
        let volume = guard::strip_verbatim(&fs::canonicalize(volume_root).unwrap_or_else(|_| {
            volume_root.to_path_buf()
        }));
        let root = match std::env::var_os("DISKDOCTOR_TRASH_DIR") {
            Some(dir) if !dir.is_empty() => {
                let mut p = PathBuf::from(dir);
                // 各卷分开，避免测试互相踩
                p.push(volume.to_string_lossy().replace([':', '\\', '/'], ""));
                p
            }
            _ => guard::trash_root_for(&volume),
        };
        Ok(Self { root, volume })
    }

    /// 按「被清理路径」自动选到正确的卷。暂存区放在**文件所在卷**上。
    pub fn for_path(path: &Path) -> Result<Self> {
        let vol = guard::volume_root_of(path)
            .ok_or_else(|| TrashError::NoVolume(path.display().to_string()))?;
        Self::open(&vol)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn volume(&self) -> &Path {
        &self.volume
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    fn ops_dir(&self) -> PathBuf {
        self.root.join("ops")
    }

    fn op_dir(&self, id: &str) -> PathBuf {
        self.ops_dir().join(id)
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.ops_dir())?;
        Ok(())
    }

    /// 把一组路径移进暂存区。
    ///
    /// 任何一条被护栏拒绝都不会导致整批失败 —— 它会被记进 `rejected`
    /// 并继续处理其余的。这样用户不会因为清单里混进一个 `C:\Windows`
    /// 就得重新挑一遍。
    pub fn stage(&self, reqs: &[StageRequest], note: &str, dry_run: bool) -> Result<StageOutcome> {
        let mut rejected: Vec<(String, String)> = Vec::new();
        let mut accepted: Vec<(PathBuf, StageRequest)> = Vec::new();

        for r in reqs {
            match guard::check(&r.path, Some(&self.root)) {
                Ok(real) => {
                    // 顺带确认它确实在这个卷上，否则 rename 会变成跨卷失败
                    match guard::volume_root_of(&real) {
                        Some(v) if v == self.volume => {
                            accepted.push((real, r.clone()));
                        }
                        Some(v) => rejected.push((
                            r.path.display().to_string(),
                            format!(
                                "不在本卷（{} 属于 {}），暂存区只能处理 {} 上的文件",
                                r.path.display(),
                                v.display(),
                                self.volume.display()
                            ),
                        )),
                        None => rejected.push((
                            r.path.display().to_string(),
                            "无法判断所在卷".to_string(),
                        )),
                    }
                }
                Err(e) => rejected.push((r.path.display().to_string(), e.to_string())),
            }
        }

        let id = new_op_id();
        let now = now_unix();

        // 先建好条目骨架（此时还不知道真实大小）
        let mut items: Vec<TrashItem> = Vec::with_capacity(accepted.len());
        for (i, (real, r)) in accepted.iter().enumerate() {
            let is_dir = real.is_dir();
            let size = if r.size > 0 {
                r.size
            } else if dry_run {
                0
            } else {
                dir_size(real)
            };
            let seq = (i + 1) as u32;
            items.push(TrashItem {
                seq,
                original_path: real.to_string_lossy().to_string(),
                stored_name: stored_name_for(seq, real),
                size,
                is_dir,
                reason: r.reason.clone(),
                rule_id: r.rule_id.clone(),
                restored: false,
                skipped_reason: None,
            });
        }

        let total_size = items.iter().map(|i| i.size).sum();
        let entry = TrashEntry {
            id: id.clone(),
            created_at: now,
            volume: self.volume.to_string_lossy().to_string(),
            status: TrashStatus::Pending,
            items,
            total_size,
            note: note.to_string(),
            purged_at: 0,
        };

        if dry_run {
            return Ok(StageOutcome {
                entry: TrashEntry {
                    status: TrashStatus::Staged,
                    ..entry
                },
                rejected,
                dry_run: true,
            });
        }

        // —— 两阶段提交 · 第一阶段：落盘计划 ——
        self.ensure_dirs()?;
        let op_dir = self.op_dir(&id);
        let data_dir = op_dir.join("data");
        fs::create_dir_all(&data_dir)?;
        self.write_manifest(&op_dir, &entry)?; // status = Pending

        // —— 第二阶段：逐项移动 ——
        //
        // **尽力而为，不是全有或全无。** 这些条目彼此独立，没有跨条目的事务
        // 语义，所以一项被占用不该拖累其余。
        //
        // 实测教训：`AppData\Local\Temp` 经常被运行中的程序占用，
        // 而它是体积最大的一项。用"全有或全无"时，它一失败就导致
        // 其余 2.6 GB 全部清不掉 —— 用户会以为工具坏了。
        let mut entry = entry;
        let mut ok_count = 0usize;

        for it in entry.items.iter_mut() {
            let from = PathBuf::from(&it.original_path);
            let to = data_dir.join(&it.stored_name);
            match fs::rename(&from, &to) {
                Ok(()) => ok_count += 1,
                Err(e) => {
                    it.skipped_reason = Some(friendly_io_error(&e));
                }
            }
        }

        // —— 提交 ——
        //
        // total_size 只统计**真正移入**的，否则报告会说谎。
        entry.total_size = entry
            .items
            .iter()
            .filter(|i| i.skipped_reason.is_none())
            .map(|i| i.size)
            .sum();
        entry.status = if ok_count == 0 {
            TrashStatus::Cancelled
        } else {
            TrashStatus::Staged
        };
        self.write_manifest(&op_dir, &entry)?;

        Ok(StageOutcome {
            entry,
            rejected,
            dry_run: false,
        })
    }

    /// 列出所有操作记录。`include_history` 为 false 时只列还占着空间的。
    pub fn list(&self, include_history: bool) -> Result<Vec<TrashEntry>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let rd = match fs::read_dir(self.ops_dir()) {
            Ok(r) => r,
            Err(_) => return Ok(out),
        };
        for e in rd.flatten() {
            let m = e.path().join("manifest.json");
            if !m.is_file() {
                continue;
            }
            match read_manifest(&m) {
                Ok(entry) => {
                    if include_history || entry.status.occupies_space() {
                        out.push(entry);
                    }
                }
                // 单个 manifest 坏了不该让整个列表失败
                Err(_) => continue,
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// 恢复一批（或全部）。
    ///
    /// 冲突处理：如果原路径已经被别的文件占了，**跳过并报告**，
    /// 而不是覆盖或改名 —— 覆盖会丢用户的新数据，改名会让用户困惑
    /// "我的文件去哪了"。跳过是最保守且最好解释的做法。
    pub fn restore(&self, op_id: Option<&str>, dry_run: bool) -> Result<Vec<RestoreOutcome>> {
        let targets: Vec<TrashEntry> = match op_id {
            Some(id) => vec![self.load(id)?],
            None => self
                .list(false)?
                .into_iter()
                .filter(|e| e.status.occupies_space())
                .collect(),
        };

        let mut outcomes = Vec::new();
        for entry in targets {
            outcomes.push(self.restore_one(entry, dry_run)?);
        }
        Ok(outcomes)
    }

    fn restore_one(&self, mut entry: TrashEntry, dry_run: bool) -> Result<RestoreOutcome> {
        // pending 状态说明上次执行中断了，此时暂存区里的东西可能不完整，
        // 不能盲目恢复
        if entry.status == TrashStatus::Pending {
            return Err(TrashError::Corrupt(format!(
                "操作 {} 处于「执行中断」状态，说明上次移动过程中断。\
                 请先手工检查 {} 下的内容再处理。",
                entry.id,
                self.op_dir(&entry.id).join("data").display()
            )));
        }
        // 从未移入过的批次，没有东西可恢复
        if entry.status == TrashStatus::Cancelled {
            return Ok(RestoreOutcome {
                op_id: entry.id,
                restored: 0,
                skipped: vec![],
            });
        }

        let data_dir = self.op_dir(&entry.id).join("data");
        let mut restored = 0usize;
        let mut skipped: Vec<(String, String)> = Vec::new();

        for it in entry.items.iter_mut() {
            if it.restored {
                continue;
            }
            // 这一项当初就没移进来，它一直在原处，不需要"恢复"
            if it.skipped_reason.is_some() {
                continue;
            }
            let from = data_dir.join(&it.stored_name);
            let to = PathBuf::from(&it.original_path);

            if !from.exists() {
                skipped.push((it.original_path.clone(), "暂存区内已不存在".into()));
                continue;
            }
            if to.exists() {
                skipped.push((
                    it.original_path.clone(),
                    "原位置已有同名文件（通常是程序在清理后自动重建的），\
                     为避免覆盖你的新数据已跳过"
                        .into(),
                ));
                continue;
            }
            // 父目录可能在这期间被删了
            if let Some(parent) = to.parent() {
                if !parent.exists() {
                    if dry_run {
                        // 预览时只报告，不创建目录
                    } else if let Err(e) = fs::create_dir_all(parent) {
                        skipped.push((
                            it.original_path.clone(),
                            format!("无法重建上级目录（{e}）"),
                        ));
                        continue;
                    }
                }
            }

            if dry_run {
                restored += 1;
                continue;
            }

            match fs::rename(&from, &to) {
                Ok(()) => {
                    it.restored = true;
                    restored += 1;
                }
                Err(e) => skipped.push((
                    it.original_path.clone(),
                    format!("恢复失败：{}", friendly_io_error(&e)),
                )),
            }
        }

        if !dry_run {
            let left = entry.staged_items().count();
            entry.status = if left == 0 {
                TrashStatus::Restored
            } else if restored > 0 {
                TrashStatus::PartiallyRestored
            } else {
                entry.status
            };
            let op_dir = self.op_dir(&entry.id);
            self.write_manifest(&op_dir, &entry)?;
        }

        Ok(RestoreOutcome {
            op_id: entry.id,
            restored,
            skipped,
        })
    }

    /// 真正删除 —— **不可逆**。
    ///
    /// `older_than_days` 只清理创建时间早于 N 天的批次，用于定期回收。
    pub fn purge(
        &self,
        op_id: Option<&str>,
        older_than_days: Option<u64>,
        dry_run: bool,
    ) -> Result<Vec<PurgeOutcome>> {
        let cutoff = older_than_days
            .map(|d| now_unix() - (d as i64) * 86_400)
            .unwrap_or(i64::MAX);

        let targets: Vec<TrashEntry> = match op_id {
            Some(id) => vec![self.load(id)?],
            None => self
                .list(false)?
                .into_iter()
                .filter(|e| e.status.occupies_space() && e.created_at <= cutoff)
                .collect(),
        };

        let mut out = Vec::new();
        for mut entry in targets {
            let status_before = entry.status;

            // 已经清理过（或从未占用空间）的批次，**什么也不做**，
            // 并如实报告 —— 不能再次声称"已释放 X GB"。
            if !entry.holds_space() {
                out.push(PurgeOutcome {
                    op_id: entry.id,
                    freed: 0,
                    item_count: 0,
                    dry_run,
                    already_cleared: true,
                    status_before,
                });
                continue;
            }

            // 只统计**真正在暂存区里**的内容 —— 未能移入的那些本来就在原处，
            // 算进来会让"释放了多少空间"这个数字虚高。
            // `remaining()` / `staged_count()` 已含状态判断，天然正确。
            let remaining = entry.remaining();
            let count = entry.staged_count();

            if dry_run {
                out.push(PurgeOutcome {
                    op_id: entry.id,
                    freed: remaining,
                    item_count: count,
                    dry_run: true,
                    already_cleared: false,
                    status_before,
                });
                continue;
            }

            let data_dir = self.op_dir(&entry.id).join("data");
            if data_dir.exists() {
                fs::remove_dir_all(&data_dir)?;
            }

            entry.status = TrashStatus::Purged;
            entry.purged_at = now_unix();
            self.write_manifest(&self.op_dir(&entry.id), &entry)?;

            out.push(PurgeOutcome {
                op_id: entry.id,
                freed: remaining,
                item_count: count,
                dry_run: false,
                already_cleared: false,
                status_before,
            });
        }
        Ok(out)
    }

    /// 汇总当前暂存区占用的空间。
    ///
    /// 只统计**真正移入**的条目 —— 字节数和条目数必须同口径，
    /// 否则会出现"2.22 GB / 163 项"这种自相矛盾的输出
    /// （163 里含 12 项没移进来的，而字节数已经排除了它们）。
    pub fn usage(&self) -> Result<(u64, usize)> {
        let mut bytes = 0u64;
        let mut n = 0usize;
        for e in self.list(false)? {
            if e.status.occupies_space() {
                bytes += e.remaining();
                n += e.staged_count();
            }
        }
        Ok((bytes, n))
    }

    /// 清除历史记录。
    ///
    /// `purge` 之后 manifest 会作为操作历史留下来（便于追溯"我什么时候清过什么"），
    /// 但它们会一直累积。这个方法是唯一可以删掉它们的途径 ——
    /// **只删已经不再占用空间的批次**，任何还有文件在暂存区里的批次都不动。
    ///
    /// 返回 (清除的批次数, 释放的目录字节数)。
    pub fn clear_history(&self, dry_run: bool) -> Result<(usize, u64)> {
        let mut count = 0usize;
        let mut bytes = 0u64;

        for entry in self.list(true)? {
            // 还有文件躺在暂存区里的，绝不碰
            if entry.status.occupies_space() {
                continue;
            }
            // Pending 说明上次执行中断，里面的东西还没理清，保留现场
            if entry.status == TrashStatus::Pending {
                continue;
            }

            let dir = self.op_dir(&entry.id);
            let size = if dry_run { 0 } else { dir_size(&dir) };
            if !dry_run {
                fs::remove_dir_all(&dir)?;
            }
            bytes += size;
            count += 1;
        }

        Ok((count, bytes))
    }

    fn load(&self, id: &str) -> Result<TrashEntry> {
        let m = self.op_dir(id).join("manifest.json");
        if !m.is_file() {
            return Err(TrashError::NoSuchOp(id.to_string()));
        }
        read_manifest(&m)
    }

    /// 原子写 manifest：先写临时文件再 rename，避免写到一半断电留下半个 JSON。
    fn write_manifest(&self, op_dir: &Path, entry: &TrashEntry) -> Result<()> {
        fs::create_dir_all(op_dir)?;
        let final_path = op_dir.join("manifest.json");
        let tmp = op_dir.join("manifest.json.tmp");

        let json = serde_json::to_vec_pretty(entry)?;
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            // 必须落盘：这份文件是断电后唯一能还原真相的依据
            f.sync_all()?;
        }
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

fn read_manifest(p: &Path) -> Result<TrashEntry> {
    let text = fs::read_to_string(p)?;
    Ok(serde_json::from_str(&text)?)
}

/// 扫描所有卷上的暂存区。用于 `trash list` 汇总展示。
pub fn all_trashes() -> Vec<Trash> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let vol = PathBuf::from(format!("{}:\\", letter as char));
        if !vol.is_dir() {
            continue;
        }
        if let Ok(t) = Trash::open(&vol) {
            if t.exists() {
                out.push(t);
            }
        }
    }
    out
}

// ------------------------------------------------------------------ 工具

fn new_op_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:09}", d.as_secs(), d.subsec_nanos())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn stored_name_for(seq: u32, path: &Path) -> String {
    let raw = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("item{seq}"));
    // 截断过长的名字，给序号和分隔符留出空间，避免撞上路径长度限制
    let mut name: String = raw.chars().take(80).collect();
    if name.is_empty() {
        name = format!("item{seq}");
    }
    // 去掉 Windows 文件名非法字符（理论上原名合法，但保险）
    name = name
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    format!("{seq:04}_{name}")
}

/// 递归求大小。清理场景下量通常不大，不必并行。
fn dir_size(p: &Path) -> u64 {
    let md = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if !md.is_dir() {
        return md.len();
    }
    let mut total = 0u64;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            total += dir_size(&e.path());
        }
    }
    total
}

/// 把 IO 错误翻译成用户能看懂的话。
fn friendly_io_error(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        PermissionDenied => "没有权限（该文件可能被系统或另一个程序占用）".into(),
        NotFound => "文件已不存在".into(),
        AlreadyExists => "目标位置已有同名文件".into(),
        _ => {
            let code = e.raw_os_error().unwrap_or(0);
            match code {
                // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
                32 | 33 => "文件正被其他程序打开，请关闭相关程序后重试".into(),
                // ERROR_ACCESS_DENIED
                5 => "拒绝访问（可能被占用或需要管理员权限）".into(),
                // ERROR_PATH_NOT_FOUND
                3 => "路径不存在".into(),
                _ => format!("{e}"),
            }
        }
    }
}
