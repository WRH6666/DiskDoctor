//! 核心数据模型：文件条目、快照、归因类别、安全等级。
//!
//! 设计要点：
//! - 快照用扁平 `Vec<Entry>` + 父节点下标表示目录树，避免每级分配 `Vec`。
//! - 不存扩展名，分析阶段按需从名字推导，省内存。
//! - 归因结果按"目录先判、文件继承"的方式挂载，避免为每个文件拼全路径。

use serde::{Deserialize, Serialize};
use std::fmt;

pub const ROOT_PARENT: u32 = u32::MAX;

/// 一个文件或目录条目。约 56 字节/条，100 万条目约 56MB。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 文件名（不含路径），目录也用它
    pub name: Box<str>,
    /// 父目录在 `Snapshot::entries` 中的下标，根为 `ROOT_PARENT`
    pub parent: u32,
    /// 逻辑大小（字节）
    pub size: u64,
    /// 物理占用（簇对齐后 / 压缩后），无法获取时等于 size
    pub alloc: u64,
    /// 最后修改时间，Unix 秒；无法获取为 0
    pub mtime: i64,
    /// 硬链接数，1 表示无额外硬链接
    pub links: u32,
    /// 是否目录
    pub is_dir: bool,
    /// 扫描阶段标记的跳过原因（无权限、被排除等）
    pub skipped: bool,
}

impl Entry {
    pub fn file_name(&self) -> &str {
        &self.name
    }

    /// 从小写扩展名（不含点）。无扩展名返回 None。
    pub fn ext(&self) -> Option<String> {
        if self.is_dir {
            return None;
        }
        let name = self.name.as_ref();
        let dot = name.rfind('.')?;
        // 跳过 ".gitignore" 这类纯隐藏文件，以及结尾是点的情况
        if dot == 0 || dot + 1 >= name.len() {
            return None;
        }
        Some(name[dot + 1..].to_ascii_lowercase())
    }
}

/// 一次扫描的完整结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// 扫描起始时间，Unix 秒
    pub started_at: i64,
    /// 扫描耗时（毫秒）
    pub elapsed_ms: u64,
    /// 根路径（便于报告展示）
    pub root_path: String,
    /// 扫描根的「去盘符绝对路径」，小写。用于路径规则匹配，见 [`Snapshot::lower_dir_path`]。
    #[serde(default)]
    pub root_prefix: String,
    /// 扁平条目表，`entries[0]` 恒为根目录
    pub entries: Vec<Entry>,
    /// 因权限或错误跳过的路径数量
    pub skipped_count: u64,
    /// 采集到的系统级"隐形占用"项
    pub specials: Vec<SpecialItem>,
}

/// `C:\` → ``，`C:\Users\me` → `users\me`，`C:\Windows` → `windows`
pub fn normalize_root_prefix(p: &str) -> String {
    let s = p.replace('/', "\\");
    let s = if s.len() >= 2 && s.as_bytes()[1] == b':' {
        &s[2..]
    } else {
        s.as_str()
    };
    s.trim_matches('\\').to_ascii_lowercase()
}

impl Snapshot {
    /// 从某条目的下标向上拼出相对路径（**不含扫描根自身**）。
    ///
    /// 必须排除下标 0：否则扫描 `C:\Users\me` 时会拼成
    /// `C:\Users\me\me\AppData\...`，用户名出现两次。
    pub fn rel_path(&self, mut idx: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        while idx != ROOT_PARENT && idx != 0 {
            let e = &self.entries[idx as usize];
            parts.push(&e.name);
            idx = e.parent;
        }
        parts.reverse();
        parts.join("\\")
    }

    /// 绝对路径。
    pub fn abs_path(&self, idx: u32) -> String {
        let rel = self.rel_path(idx);
        if rel.is_empty() {
            self.root_path.clone()
        } else {
            format!("{}\\{}", self.root_path.trim_end_matches('\\'), rel)
        }
    }

    /// 供路径规则匹配的字符串：`root_prefix` + 相对路径，全小写。
    ///
    /// 为什么不只用「相对扫描根的路径」：那样扫描 `C:\Windows` 时
    /// `windows-temp` 这类规则就永远匹配不上了。拼上根前缀之后三种扫描
    /// 方式结果一致：
    ///   扫 `C:\`          → `users\me\appdata\local\temp`
    ///   扫 `C:\Users\me`  → `users\me\appdata\local\temp`
    ///   扫 `C:\Windows`   → `windows\temp`
    pub fn lower_dir_path(&self, idx: u32) -> String {
        let rel = self.rel_path_lower(idx);
        if rel.is_empty() {
            self.root_prefix.clone()
        } else if self.root_prefix.is_empty() {
            rel
        } else {
            format!("{}\\{}", self.root_prefix, rel)
        }
    }

    fn rel_path_lower(&self, mut idx: u32) -> String {
        let mut parts: Vec<String> = Vec::new();
        while idx != ROOT_PARENT && idx != 0 {
            let e = &self.entries[idx as usize];
            parts.push(e.name.to_ascii_lowercase());
            idx = e.parent;
        }
        parts.reverse();
        parts.join("\\")
    }

    /// 计算 `root_prefix` 并完成目录体积累加。新建快照后调用一次即可。
    pub fn finalize(&mut self) {
        self.root_prefix = normalize_root_prefix(&self.root_path);
        self.rollup();
    }

    /// 把所有后代的大小向上累加到祖先目录（原地修改 `size`/`alloc`）。
    ///
    /// 逆序遍历是可行的：父节点下标恒小于子节点下标（构建时保证）。
    pub fn rollup(&mut self) {
        let mut dir_size = vec![0u64; self.entries.len()];
        let mut dir_alloc = vec![0u64; self.entries.len()];
        for (i, e) in self.entries.iter().enumerate() {
            if !e.is_dir {
                dir_size[i] = e.size;
                dir_alloc[i] = e.alloc;
            }
        }
        for i in (1..self.entries.len()).rev() {
            let p = self.entries[i].parent;
            if p != ROOT_PARENT {
                let s = dir_size[i];
                let a = dir_alloc[i];
                dir_size[p as usize] += s;
                dir_alloc[p as usize] += a;
            }
        }
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.is_dir {
                e.size = dir_size[i];
                e.alloc = dir_alloc[i];
            }
        }
    }

    pub fn total_size(&self) -> u64 {
        self.entries.first().map(|e| e.size).unwrap_or(0)
    }

    pub fn total_alloc(&self) -> u64 {
        self.entries.first().map(|e| e.alloc).unwrap_or(0)
    }

    pub fn file_count(&self) -> u64 {
        self.entries.iter().filter(|e| !e.is_dir).count() as u64
    }

    pub fn dir_count(&self) -> u64 {
        self.entries.iter().filter(|e| e.is_dir).count() as u64
    }
}

/// 归因类别：把一堆路径映射成人类能理解的"这是什么"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    // —— 可安全回收 ——
    PackageCache,
    BuildArtifact,
    IdeCache,
    BrowserCache,
    TempFile,
    CrashDump,
    LogFile,
    OldInstaller,
    OsUpdateLeftover,
    RecycleBin,
    ThumbnailCache,
    VirtualDisk,

    // —— 系统管理，只能走官方命令 ——
    HibernationFile,
    PageFile,
    ShadowCopy,
    ComponentStore,
    DriverStore,

    // —— 用户数据，默认不动 ——
    UserDocument,
    UserMedia,
    UserDownload,
    GameData,
    AppData,
    ImApp,

    Unknown,
}

impl Category {
    /// 稳定的机器可读标识（与 serde 序列化的字符串一致）。
    /// 与 `Safety::key` 同理，由 `keys_match_serde` 测试保证一致性。
    pub fn key(&self) -> &'static str {
        match self {
            Category::PackageCache => "package-cache",
            Category::BuildArtifact => "build-artifact",
            Category::IdeCache => "ide-cache",
            Category::BrowserCache => "browser-cache",
            Category::TempFile => "temp-file",
            Category::CrashDump => "crash-dump",
            Category::LogFile => "log-file",
            Category::OldInstaller => "old-installer",
            Category::OsUpdateLeftover => "os-update-leftover",
            Category::RecycleBin => "recycle-bin",
            Category::ThumbnailCache => "thumbnail-cache",
            Category::VirtualDisk => "virtual-disk",
            Category::HibernationFile => "hibernation-file",
            Category::PageFile => "page-file",
            Category::ShadowCopy => "shadow-copy",
            Category::ComponentStore => "component-store",
            Category::DriverStore => "driver-store",
            Category::UserDocument => "user-document",
            Category::UserMedia => "user-media",
            Category::UserDownload => "user-download",
            Category::GameData => "game-data",
            Category::AppData => "app-data",
            Category::ImApp => "im-app",
            Category::Unknown => "unknown",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Category::PackageCache => "包管理器缓存",
            Category::BuildArtifact => "构建产物",
            Category::IdeCache => "IDE / 编辑器缓存",
            Category::BrowserCache => "浏览器缓存",
            Category::TempFile => "临时文件",
            Category::CrashDump => "崩溃转储",
            Category::LogFile => "日志文件",
            Category::OldInstaller => "旧安装包",
            Category::OsUpdateLeftover => "系统更新残留",
            Category::RecycleBin => "回收站",
            Category::ThumbnailCache => "缩略图 / 字体缓存",
            Category::VirtualDisk => "虚拟机 / 容器磁盘镜像",
            Category::HibernationFile => "休眠文件",
            Category::PageFile => "虚拟内存页面文件",
            Category::ShadowCopy => "系统还原点 / 卷影副本",
            Category::ComponentStore => "组件存储 WinSxS",
            Category::DriverStore => "驱动仓库",
            Category::UserDocument => "个人文档",
            Category::UserMedia => "个人影音图片",
            Category::UserDownload => "下载目录",
            Category::GameData => "游戏数据",
            Category::AppData => "应用数据",
            Category::ImApp => "聊天 / 办公软件数据",
            Category::Unknown => "未识别",
        }
    }
}

/// 安全等级：决定这个条目能不能被"一键清理"碰到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Safety {
    /// 可安全回收，删了能自动重建 / 重新下载
    Safe,
    /// 需要你确认，可能含个人数据
    Caution,
    /// 用户数据，本工具不主动清理
    Keep,
    /// 系统托管：禁止直接删，只能通过官方命令
    SystemManaged,
    /// 硬黑名单：永不触碰
    Protected,
}

impl Safety {
    /// 稳定的机器可读标识（与 serde 序列化的字符串一致）。
    ///
    /// 存在理由是性能：生成界面时要给成百上千条打标签，
    /// 走序列化会有分配开销。但手工维护的映射容易在新增变体时漏掉，
    /// 所以有 `keys_match_serde` 测试锁住两者一致。
    pub fn key(&self) -> &'static str {
        match self {
            Safety::Safe => "safe",
            Safety::Caution => "caution",
            Safety::Keep => "keep",
            Safety::SystemManaged => "system-managed",
            Safety::Protected => "protected",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Safety::Safe => "可安全回收",
            Safety::Caution => "需确认",
            Safety::Keep => "用户数据",
            Safety::SystemManaged => "系统托管",
            Safety::Protected => "禁止触碰",
        }
    }

    /// 归因评分：0 = 绝对不能删，100 = 完全放心删。
    /// 用于给每个条目一个可排序的"可删分"。
    pub fn score(&self) -> u8 {
        match self {
            Safety::Safe => 92,
            Safety::Caution => 55,
            Safety::Keep => 20,
            Safety::SystemManaged => 8,
            Safety::Protected => 0,
        }
    }
}

impl fmt::Display for Safety {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// 处置方式：说清"该怎么处理"，而不只是"能不能删"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// 移入 DiskDoctor 自己的暂存区（**不是**系统回收站），可一键撤销。
    ///
    /// 刻意与系统回收站区分：暂存区由本工具管理，能精确记录每一项的来源、
    /// 体积、清理理由，恢复时不覆盖同名文件。而系统回收站我们只读取、
    /// 绝不代为操作（手工搬动会破坏它的索引结构）。
    Delete,
    /// 执行官方命令（模板内含占位符）
    Command { cmd: String },
    /// 压缩虚拟磁盘
    Compact,
    /// 只能人工复核
    Review,
    /// 不做任何事
    None,
}

/// 系统级"隐形占用"：不在常规目录树里，或需要特权才能读到的东西。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialItem {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub category: Category,
    pub safety: Safety,
    pub action: Action,
    /// 为什么这样处理
    pub rationale: String,
    /// 回收之后的后果 / 怎么恢复
    pub recovery: String,
    /// 数据来源说明（命令或 API），保证可追溯
    pub source: String,
    /// 是否成功采到数据
    pub ok: bool,
}

impl SpecialItem {
    pub fn metrics(&self) -> u64 {
        self.size
    }

    /// 给用户看的「该怎么办」，而不是内部的动作枚举。
    ///
    /// 刻意放在 dd-core 而不是报告层：这是模型自身的语义，
    /// 而且 inherent impl 必须和类型在同一个 crate 里。
    pub fn action_text(&self) -> String {
        match &self.action {
            Action::Command { cmd } => format!("执行 `{cmd}`"),
            Action::Compact => "压缩（不丢数据）".into(),
            Action::Delete => "可清理".into(),
            Action::Review => "人工确认后处理".into(),
            Action::None => "不需要处理".into(),
        }
    }
}

/// 卷（分区）的空间信息，用于报告抬头。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// 形如 `C:\`
    pub root: String,
    pub total: u64,
    pub free: u64,
}

impl VolumeInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    pub fn used_ratio(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }
}
