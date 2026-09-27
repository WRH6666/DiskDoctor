//! 采集层：把磁盘上的文件和目录变成一张扁平条目表。
//!
//! 后端策略（按优先级尝试，失败自动降级）：
//! 1. `mft`  —— NTFS 下直读 `$MFT`，秒级完成。见 `ntfs` 模块。
//! 2. `walk` —— 通用并行遍历，跨平台兜底。
//!
//! 两种后端产出同一份 [`dd_core::Snapshot`]，上层完全无感。

pub mod special;
pub mod walk;

#[cfg(windows)]
pub mod ntfs;

use dd_core::Snapshot;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// 直读 NTFS 主文件表
    Mft,
    /// 通用目录遍历
    Walk,
}

impl Backend {
    pub fn label(&self) -> &'static str {
        match self {
            Backend::Mft => "NTFS MFT 直读",
            Backend::Walk => "并行目录遍历",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("路径不存在: {0}")]
    NotFound(String),
    #[error("没有读取权限，请以管理员身份运行: {0}")]
    PermissionDenied(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("该卷不是 NTFS，无法使用 MFT 直读: {0}")]
    NotNtfs(String),
    #[error("MFT 解析失败: {0}")]
    Mft(String),
}

pub type Result<T> = std::result::Result<T, ScanError>;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 是否尝试 MFT 直读（仅 Windows + NTFS 有效）
    pub try_mft: bool,
    /// 遍历时跳过的目录名（小写）
    pub exclude_names: Vec<String>,
    /// 是否跟随符号链接 / 目录联接（默认不跟随，避免环）
    pub follow_links: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            try_mft: true,
            exclude_names: Vec::new(),
            follow_links: false,
        }
    }
}

/// 扫描一个路径。自动选择后端。
pub fn scan(path: &Path, opts: &ScanOptions) -> Result<Snapshot> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ScanError::NotFound(path.display().to_string()),
        std::io::ErrorKind::PermissionDenied => {
            ScanError::PermissionDenied(path.display().to_string())
        }
        _ => ScanError::Io(e),
    })?;
    if !meta.is_dir() {
        return Err(ScanError::NotFound(format!(
            "{} 不是目录",
            path.display()
        )));
    }

    // 优先尝试 MFT 直读
    #[cfg(windows)]
    if opts.try_mft {
        match ntfs::try_scan_mft(path, opts) {
            Ok(Some(snap)) => return Ok(snap),
            Ok(None) => { /* 非 NTFS 卷，降级 */ }
            Err(_e) => { /* 权限不足或解析失败，静默降级到遍历 */ }
        }
    }

    walk::scan_dir(path, opts)
}

/// 扫描一个路径，并额外采集系统级"隐形占用"。
pub fn scan_with_specials(path: &Path, opts: &ScanOptions) -> Result<Snapshot> {
    let mut snap = scan(path, opts)?;
    snap.specials = special::collect(path);
    Ok(snap)
}
