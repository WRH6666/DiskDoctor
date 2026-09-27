//! NTFS `$MFT` 直读后端。
//!
//! # 现状
//!
//! **本模块目前是可运行的占位实现**：`try_scan_mft` 恒返回 `Ok(None)`，
//! 上层会自动降级到 [`crate::walk`] 的并行遍历后端。
//!
//! 之所以先留接口不写实现，是因为 MFT 直读必须**以管理员身份**才能打开
//! `\\.\C:` 卷句柄，无法在无提权环境下验证。这份代码涉及二进制结构解析
//! （引导扇区、属性链表、run list、sector fixup），一旦有 bug 会给出
//! **静默错误的空间数字** —— 对一个用来指导删文件的工具来说，这是
//! 最不能接受的失败模式。因此宁可先跑通并验证遍历后端，再在有提权、
//! 能对着真实卷反复验证的条件下接入 MFT。
//!
//! # 实现计划（接入时按此顺序）
//!
//! 1. **打开卷**：`CreateFileW(r"\\.\C:", GENERIC_READ,
//!    FILE_SHARE_READ | FILE_SHARE_WRITE, OPEN_EXISTING)`。失败即无权限，
//!    返回 `Ok(None)` 走降级，不要报错打断用户。
//! 2. **解析引导扇区**：`0x0B` 每扇区字节数、`0x0D` 每簇扇区数、
//!    `0x28` 总扇区数、`0x30` `$MFT` 起始簇、`0x40` 每记录簇数
//!    （**有符号**，负数表示 `2^(-n)` 字节）。
//! 3. **定位 `$MFT` 数据**：读记录 0，取无名 `0x80` 属性，解 run list
//!    得到 MFT 自身的簇区间。
//! 4. **批量读取**：按区间一次读大块（NVMe 4MB / SSD 2MB / HDD 1MB），
//!    参考 `uffs-mft` 的做法：查询卷的 `FILE_STORAGE_INFO` 判断介质类型，
//!    据此决定并发度与块大小。只读 MFT 记录位图标记为"已用"的记录，
//!    可省下 30%–50% 的 I/O。
//! 5. **Sector fixup（关键，漏了就全是脏数据）**：每条记录 `0x04` 是 USA
//!    偏移、`0x06` 是 USA 项数。除第 0 项外，每个 512 字节扇区的末 2 字节
//!    被替换成了 USN，必须用 USA 里的原值还原回去。
//! 6. **遍历属性链**：`0x10` 标准信息（属性标志、时间戳）、
//!    `0x30` 文件名（**父目录 MFT 引用、名字、分配大小、真实大小**）。
//!    跳过 `base record reference != 0` 的扩展记录，否则同一文件会被算两次。
//! 7. **拼树**：用 `父引用 + 自身引用` 建边；注意硬链接会让一个 MFT 记录
//!    对应多个路径，必须按记录号去重，否则总量会虚高 —— WizTree 正是靠
//!    正确处理硬链接才做到"总占用和资源管理器一致"。
//!
//! # 已知边界
//!
//! - 仅 NTFS。FAT32 / exFAT / 网络盘一律返回 `Ok(None)` 走遍历。
//! - 需要管理员权限；应用内应做 UAC 提权，失败则降级并**在界面上说明**
//!   当前用的是慢速后端，不要把降级藏起来。
//! - `$MFT` 很大的时候内存要留出几百 MB 量级。

use crate::{Result, ScanOptions};
use dd_core::Snapshot;
use std::path::Path;

/// 判断该路径是否适合走 MFT 直读。
///
/// 仅接受形如 `C:\` 的整卷根；子目录无法用 MFT 直读（MFT 是全卷的）。
pub fn can_use_mft(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let s = path.to_string_lossy();
    let bytes = s.as_bytes();
    // 只接受 "X:\" 或 "X:/" 这种两三个字符的盘根
    if bytes.len() > 3 {
        return false;
    }
    if bytes.len() < 2 || bytes[1] != b':' {
        return false;
    }
    bytes[0].is_ascii_alphabetic()
}

/// 尝试用 MFT 直读扫描整卷。
///
/// - `Ok(Some(snapshot))` —— 成功
/// - `Ok(None)` —— 环境不支持（非 NTFS / 无管理员权限），调用方应降级
/// - `Err(_)` —— 明确失败，调用方同样降级
pub fn try_scan_mft(path: &Path, _opts: &ScanOptions) -> Result<Option<Snapshot>> {
    if !can_use_mft(path) {
        return Ok(None);
    }
    // 尚未实现，按契约返回"不支持"，由上层降级到并行遍历。
    Ok(None)
}
