//! 系统级"隐形占用"采集。
//!
//! 这一类东西是**普通磁盘分析工具的最大盲区**：它们要么在资源管理器里
//! 根本不计入占用（WSL / Docker 的 vhdx），要么因为权限根本扫不到
//! （`System Volume Information` 里的卷影副本），要么看得见但不敢碰
//! （`hiberfil.sys`）。而在 Windows 上，它们合计常常就是几十 GB。
//!
//! 这里做两件事：
//! 1. 把它们的体积量出来；
//! 2. 更重要的是，给出**正确的处理方式** —— 是"删"、是"执行某条命令"，
//!    还是"绝对不要碰"。这正是分析类工具缺失的那一层。

use dd_core::{Action, Category, Safety, SpecialItem, VolumeInfo};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 枚举当前机器上所有可用的卷根（`C:\`、`D:\` ...）。
pub fn volume_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        let p = PathBuf::from(&root);
        // 用 read_dir 而不是 metadata：光驱空了的时候 metadata 也会成功，
        // 但不能列目录，用它判断更准。
        if fs::read_dir(&p).is_ok() {
            out.push(p);
        }
    }
    out
}

pub fn volume_infos() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    #[cfg(windows)]
    for root in volume_roots() {
        if let Some((total, free)) = win::space(&root) {
            out.push(VolumeInfo {
                root: root.display().to_string(),
                total,
                free,
            });
        }
    }
    #[cfg(not(windows))]
    for root in volume_roots() {
        let _ = root;
    }
    out
}

/// 采集与 `scan_root` 同卷相关的系统级隐形占用。
pub fn collect(scan_root: &Path) -> Vec<SpecialItem> {
    let mut out = Vec::new();

    // 页面文件/休眠文件可能分布在多个卷上，所以把每个卷都看一遍
    for vol in volume_roots() {
        let files = root_files(&vol);
        push_volume_file(&mut out, &vol, &files, "hiberfil.sys");
        push_volume_file(&mut out, &vol, &files, "pagefile.sys");
        push_volume_file(&mut out, &vol, &files, "swapfile.sys");
    }

    out.extend(shadow_storage());
    let _ = scan_root;
    out
}

/// 列出某个卷根目录下的文件 → 大小（小写文件名做键）。
fn root_files(vol: &Path) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    if let Ok(rd) = fs::read_dir(vol) {
        for e in rd.flatten() {
            let key = e.file_name().to_string_lossy().to_ascii_lowercase();
            // hiberfil.sys 这类受保护文件，枚举时拿不到 metadata 是正常的
            let size = e.metadata().map(|md| md.len()).unwrap_or(0);
            m.insert(key, size);
        }
    }
    m
}

fn push_volume_file(
    out: &mut Vec<SpecialItem>,
    vol: &Path,
    files: &HashMap<String, u64>,
    name: &str,
) {
    let Some(&size) = files.get(name) else {
        return;
    };
    let path = format!("{}{}", vol.display(), name);
    let item = match name {
        "hiberfil.sys" => SpecialItem {
            name: "休眠文件".into(),
            path,
            size,
            category: Category::HibernationFile,
            safety: Safety::SystemManaged,
            action: Action::Command {
                cmd: "powercfg /h off".into(),
            },
            rationale: "系统休眠时把内存内容写到这里，体积约等于物理内存容量。\
                        执行 powercfg /h off 可以让 Windows 自己删除这个文件。\
                        注意：这同时会关掉「快速启动」，开机会变慢一点。"
                .into(),
            recovery: "执行 powercfg /h on 即可恢复，文件会按需重新生成。".into(),
            source: "目录枚举".into(),
            ok: size > 0,
        },
        "pagefile.sys" => SpecialItem {
            name: "虚拟内存页面文件".into(),
            path,
            size,
            category: Category::PageFile,
            safety: Safety::SystemManaged,
            action: Action::Review,
            rationale: "内存不够时系统把不常用的数据换到这里。\
                        **绝对不要直接删除**，删了会导致程序崩溃甚至蓝屏。\
                        想省空间应该把它迁到空闲的盘，或设成较小的固定值。"
                .into(),
            recovery: "在「系统属性 → 高级 → 性能 → 虚拟内存」里改回来。".into(),
            source: "目录枚举".into(),
            ok: size > 0,
        },
        _ => SpecialItem {
            name: "UWP 交换文件".into(),
            path,
            size,
            category: Category::PageFile,
            safety: Safety::SystemManaged,
            action: Action::None,
            rationale: "供商店应用使用的交换文件，由 system 进程独占，无法手动删除，\
                        也不需要处理。列在这里只是为了让你看到这几十 GB 去哪了。"
                .into(),
            recovery: "系统自动管理。".into(),
            source: "目录枚举".into(),
            ok: size > 0,
        },
    };
    out.push(item);
}

/// 查询卷影副本（系统还原点）占用。
///
/// 用 WMI 的 `Win32_ShadowStorage` 而不是 `vssadmin list shadowstorage`：
/// 后者的输出是本地化的，中文系统上字段名会变成「已用/分配」，
/// 解析起来极不可靠。WMI 的属性名永远是英文。
fn shadow_storage() -> Vec<SpecialItem> {
    let ps = "Get-CimInstance -ClassName Win32_ShadowStorage -ErrorAction Stop \
              | Select-Object -Property UsedSpace,AllocatedSpace,MaxSpace \
              | ConvertTo-Json -Compress";

    let output = run_hidden("powershell", &["-NoProfile", "-NonInteractive", "-Command", ps]);

    let (size, ok, note) = match output {
        Some(stdout) => match parse_shadow_json(&stdout) {
            Some(used) => (used, true, String::new()),
            None => (0, false, "返回内容无法解析。".to_string()),
        },
        None => (
            0,
            false,
            "查询失败，通常是没有管理员权限。请以管理员身份重新运行以获取这项数据。".into(),
        ),
    };

    vec![SpecialItem {
        name: "系统还原点 / 卷影副本".into(),
        path: "System Volume Information".into(),
        size,
        category: Category::ShadowCopy,
        safety: Safety::SystemManaged,
        action: Action::Review,
        rationale: format!(
            "由卷影复制服务（VSS）维护的历史快照，用于系统还原和文件「以前的版本」。\
             这是很多人 C 盘莫名少几十 GB 的真正原因 —— 而它在资源管理器里几乎看不见，\
             普通扫描工具也读不到。{note}"
        ),
        recovery: "删除旧还原点后无法再回退到那些时间点，但系统会按计划继续创建新的。\
                   建议保留最近的还原点，只清理更早的。"
            .into(),
        source: "WMI Win32_ShadowStorage".into(),
        ok,
    }]
}

fn parse_shadow_json(text: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let mut total = 0u64;
    match &v {
        // 只有一个卷影存储时 WMI 返回对象，多个时返回数组，两种都要处理
        serde_json::Value::Array(items) => {
            for it in items {
                total += it.get("UsedSpace").and_then(|x| x.as_u64()).unwrap_or(0);
            }
        }
        serde_json::Value::Object(_) => {
            total += v.get("UsedSpace").and_then(|x| x.as_u64()).unwrap_or(0);
        }
        _ => return None,
    }
    Some(total)
}

/// 执行外部命令并拿到 stdout，不弹出控制台窗口。
fn run_hidden(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    // 同 walk.rs：手工 FFI，避免 windows-sys 触发 dlltool 生成 import library。
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp_directory_name: *const u16,
            lp_free_bytes_available_to_caller: *mut u64,
            lp_total_number_of_bytes: *mut u64,
            lp_total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    /// 返回 (总量, 可用量)。
    pub fn space(root: &Path) -> Option<(u64, u64)> {
        let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
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
        Some((total, total_free))
    }
}
