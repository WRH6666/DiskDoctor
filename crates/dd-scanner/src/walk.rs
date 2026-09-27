//! 通用并行遍历后端。
//!
//! # 为什么不用递归
//!
//! 递归下降写起来最直观，但有两个问题：深目录会爆栈（`dust` 就为此专门
//! 把递归改成 `rayon::scope` 并去掉了栈大小参数），以及无法保证
//! "父节点先于子节点写入"。
//!
//! 这里改用**逐层并行的广度优先**：每一层的目录并行读取，读完后统一
//! 追加到条目表。这样天然满足「父节点下标 < 子节点下标」，
//! [`dd_core::Snapshot::rollup`] 只要倒着扫一遍就能算出所有目录的总量，
//! 不需要递归、不需要额外索引。
//!
//! 代价是每层有一次同步屏障，遇到"某一层里有个超大目录"时会等它。
//! 这正是 MFT 后端要解决的问题 —— 但那是数量级的差别，不是这里的常数差别。

use crate::{Result, ScanOptions};
use dd_core::{Entry, Snapshot, ROOT_PARENT};
use rayon::prelude::*;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 每批并行处理的目录数，用来限制内存峰值（避免一次性持有整层的结果）。
const CHUNK: usize = 2048;
/// 目录深度上限，纯粹是防御性的，正常文件系统远到不了。
const MAX_DEPTH: u32 = 512;

struct RawChild {
    name: Box<str>,
    size: u64,
    alloc: u64,
    mtime: i64,
    links: u32,
    is_dir: bool,
    /// 是否需要继续往下走（目录且不是重解析点）
    descend: bool,
}

pub fn scan_dir(root: &Path, opts: &ScanOptions) -> Result<Snapshot> {
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let t0 = std::time::Instant::now();

    let root_md = fs::symlink_metadata(root)?;

    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| root.display().to_string());

    let mut entries: Vec<Entry> = Vec::with_capacity(8192);
    entries.push(Entry {
        name: root_name.into_boxed_str(),
        parent: ROOT_PARENT,
        size: 0,
        alloc: 0,
        mtime: mtime_of(&root_md),
        links: links_of(&root_md),
        is_dir: true,
        skipped: false,
    });

    let mut skipped_total = 0u64;
    let mut frontier: Vec<(u32, PathBuf)> = vec![(0, root.to_path_buf())];
    let mut depth = 0u32;

    while !frontier.is_empty() && depth < MAX_DEPTH {
        depth += 1;
        let mut next: Vec<(u32, PathBuf)> = Vec::with_capacity(frontier.len());

        for chunk in frontier.chunks(CHUNK) {
            let results: Vec<(Vec<RawChild>, u64)> = chunk
                .par_iter()
                .map(|(_, p)| read_children(p, opts))
                .collect();

            for ((parent_idx, parent_path), (children, skipped)) in chunk.iter().zip(results) {
                skipped_total += skipped;
                for c in children {
                    let idx = entries.len() as u32;
                    // 只有需要继续深入时才拼路径，避免为每个文件分配 PathBuf
                    let child_path = if c.descend {
                        Some(parent_path.join(c.name.as_ref()))
                    } else {
                        None
                    };
                    entries.push(Entry {
                        name: c.name,
                        parent: *parent_idx,
                        size: c.size,
                        alloc: c.alloc,
                        mtime: c.mtime,
                        links: c.links,
                        is_dir: c.is_dir,
                        skipped: false,
                    });
                    if let Some(p) = child_path {
                        next.push((idx, p));
                    }
                }
            }
        }

        frontier = next;
    }

    let mut snap = Snapshot {
        started_at,
        elapsed_ms: 0,
        root_path: root.display().to_string(),
        root_prefix: String::new(),
        entries,
        skipped_count: skipped_total,
        specials: Vec::new(),
    };
    snap.finalize();
    snap.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(snap)
}

/// 读取一个目录的直接子项。返回 (子项, 读取失败的条目数)。
fn read_children(dir: &Path, opts: &ScanOptions) -> (Vec<RawChild>, u64) {
    let mut out = Vec::new();
    let mut skipped = 0u64;

    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        // 无权限或已被删除，计一次跳过，不影响整体扫描
        Err(_) => return (out, 1),
    };

    for ent in rd {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let name_os = ent.file_name();
        let name = name_os.to_string_lossy();

        if !opts.exclude_names.is_empty() {
            let lower = name.to_ascii_lowercase();
            if opts.exclude_names.iter().any(|x| *x == lower) {
                continue;
            }
        }

        let md = match ent.metadata() {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let attrs = attrs_of(&md);
        let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let is_dir = md.is_dir();

        // 不跟随重解析点（符号链接 / 目录联接）。Windows 上目录联接极常见
        // （比如 C:\Users\All Users），跟进去会陷入环，也会把同一份数据算两次。
        let descend = is_dir && !is_reparse;

        let (size, alloc) = if is_dir {
            (0, 0)
        } else {
            (md.len(), alloc_size(dir, &name_os, &md))
        };

        out.push(RawChild {
            name: name.into_owned().into_boxed_str(),
            size,
            alloc,
            mtime: mtime_of(&md),
            links: links_of(&md),
            is_dir,
            descend,
        });
    }

    (out, skipped)
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;

fn mtime_of(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn attrs_of(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

#[cfg(not(windows))]
fn attrs_of(_md: &fs::Metadata) -> u32 {
    0
}

/// 硬链接数。
///
/// **遍历后端一律返回 1**，这是刻意的取舍：
///
/// 拿到真实的硬链接数只能靠 `GetFileInformationByHandle`，而那需要先为
/// 每个文件打开一个句柄 —— 在百万级文件上这是不可接受的开销，会让遍历
/// 后端慢到失去意义。
///
/// 硬链接去重是 **MFT 后端的职责**：NTFS 的每条文件记录在标准信息属性
/// （`0x10`）里直接带着 `LinkCount` 和文件引用号，读到就是免费的，
/// 按引用号去重即可。WizTree 能做到"总占用和资源管理器一致"正是靠这个。
///
/// 影响面：只有 Windows 系统目录（`WinSxS` 这类硬链接仓库）会产生明显
/// 偏差，普通用户数据里几乎不存在硬链接。MFT 后端接入后此问题自然消失。
#[cfg(windows)]
fn links_of(_md: &fs::Metadata) -> u32 {
    1
}

#[cfg(not(windows))]
fn links_of(md: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    // Unix 上 nlink 是 stat 的常规字段，读取无额外代价，可以用真实值
    md.nlink() as u32
}

/// 物理占用。
///
/// 绝大多数文件逻辑大小 ≈ 物理占用，为每个文件多打一次 Win32 调用不划算。
/// 只有**稀疏文件**和**压缩文件**才会明显偏离（虚拟磁盘镜像、Docker/WSL 的
/// vhdx 就属于这类，恰恰是用户最想看清的一类），所以只对它们求真实占用。
fn alloc_size(dir: &Path, name: &OsStr, md: &fs::Metadata) -> u64 {
    #[cfg(windows)]
    {
        let attrs = attrs_of(md);
        if attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0 || attrs & FILE_ATTRIBUTE_COMPRESSED != 0 {
            if let Some(s) = win::compressed_size(&dir.join(name)) {
                return s;
            }
        }
    }
    let _ = (dir, name);
    md.len()
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    // 刻意手工声明 FFI，而不是依赖 windows-sys：
    // windows-sys 会为引用的每个 DLL 触发 dlltool 现场生成 import library，
    // 在 GNU 工具链上这一步很容易失败（缺 dlltool 或其运行时 DLL）。
    // 直接 #[link] 到 kernel32，用 MinGW 自带的 libkernel32.a 即可 ——
    // 少一个依赖，少一个失败点，而且这两个函数签名几十年没变过。
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCompressedFileSizeW(lp_file_name: *const u16, lp_file_size_high: *mut u32) -> u32;
    }

    /// `GetCompressedFileSizeW` 返回文件在磁盘上的实际占用（簇对齐后），
    /// 对稀疏/压缩文件会明显小于逻辑大小。失败返回 `None`。
    pub fn compressed_size(path: &Path) -> Option<u64> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut high: u32 = 0;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low == u32::MAX {
            return None;
        }
        Some(((high as u64) << 32) | low as u64)
    }
}
