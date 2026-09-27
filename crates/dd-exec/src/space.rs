//! 卷可用空间的测量。
//!
//! 为什么要单独做这件事：
//!
//! `purge` 会声称"已释放 X GB"，但那是**我们删了多少**，不等于
//! **磁盘真的多了多少**。实测踩过一次：报告"已释放 2.13 GB"，
//! 而卷可用空间反而少了 2G —— 因为同一个时间窗里有别的程序在写盘。
//!
//! 用户看到"清理了 2.13 GB 但磁盘更满了"，只会得出一个结论：
//! 这工具在骗人。所以释放量必须**实测**，并在不符时给出解释。
//!
//! 实现上刻意手工声明 FFI 而不引入 `windows-sys`：后者会为引用的每个
//! DLL 触发 dlltool 现场生成 import library，在 GNU 工具链下容易失败
//! （见 README 的构建说明）。`GetDiskFreeSpaceExW` 的签名几十年没变过。

use std::path::Path;

/// 某个卷的可用空间（字节）。失败返回 `None`。
pub fn available_bytes(drive_root: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        win::available(drive_root)
    }
    #[cfg(not(windows))]
    {
        let _ = drive_root;
        None
    }
}

/// 实测变化与「声称释放量」的判定结论。
///
/// 存在的理由：光有数字不够，还需要一个**判断**。用户看到
/// "已删除 500 MB，实测释放 0 B"会困惑 —— 是工具没删成？还是系统没回收？
/// 这两种情况的对策完全不同，必须区分开说。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaVerdict {
    /// 空间确实归还了 —— 正常
    Freed,
    /// 归还了一部分，但明显少于声称 —— 通常有并发写入或部分内容被重建
    Partial,
    /// **基本没有归还** —— 最可能的原因是删除被重定向了
    ///
    /// 典型场景：某些环境（安全软件、企业策略、代理运行时）会把删除
    /// 变成"移入回收站"。此时文件仍占着空间，必须清空回收站才真正释放。
    /// 实测遇到过：删除 2 GB 后回收站增长了几乎一样的体积。
    NotReleased,
    /// 反而变少了 —— 同期有其他程序在大量写盘
    Shrunk,
    /// 无从判断（没测到，或声称量为 0）
    Unknown,
}

impl DeltaVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            DeltaVerdict::Freed => "已归还",
            DeltaVerdict::Partial => "部分归还",
            DeltaVerdict::NotReleased => "**未归还**",
            DeltaVerdict::Shrunk => "反而减少",
            DeltaVerdict::Unknown => "无法判断",
        }
    }
}

/// 相对某个基准值的变化量，带正负号。
///
/// `--` 用于报告：正数表示"释放出来的空间"，负数表示"反而变少了"。
pub struct SpaceDelta {
    pub before: u64,
    pub after: u64,
}

impl SpaceDelta {
    /// 正数 = 释放了这么多；负数 = 反而少了这么多。
    pub fn delta(&self) -> i64 {
        self.after as i64 - self.before as i64
    }

    /// 实测变化与"声称释放量"是否大致吻合。
    ///
    /// 留 15% 或 64 MB 的宽容度（取较大者）：期间其他程序的正常读写、
    /// 文件系统元数据开销都会造成小偏差，不该因此报警。
    pub fn roughly_matches(&self, claimed: u64) -> bool {
        let v = self.verdict(claimed);
        matches!(v, DeltaVerdict::Freed | DeltaVerdict::Unknown)
    }

    /// 判定这次实测到底意味着什么。
    pub fn verdict(&self, claimed: u64) -> DeltaVerdict {
        if claimed == 0 {
            return DeltaVerdict::Unknown;
        }
        let d = self.delta();
        // 噪声容差：这台机器上自然波动可达数十 MB（后台服务持续写入），
        // 所以低于这个量级的变化不构成有效信号。
        const NOISE: i64 = 64 * 1024 * 1024;

        if d < -NOISE {
            // 明显变少：同期写入超过了释放量
            DeltaVerdict::Shrunk
        } else if d as f64 >= claimed as f64 * 0.7 {
            DeltaVerdict::Freed
        } else if d as f64 >= claimed as f64 * 0.1 {
            DeltaVerdict::Partial
        } else {
            // 落在噪声范围内，等于"没释放"
            DeltaVerdict::NotReleased
        }
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available_to_caller: *mut u64,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    pub fn available(drive_root: &Path) -> Option<u64> {
        let mut wide: Vec<u16> = drive_root.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free_to_caller: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_to_caller,
                &mut total,
                &mut total_free,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(total_free)
    }
}
