//! 安全护栏：最后一道防线。
//!
//! # 为什么这里不依赖规则库
//!
//! 规则库是**可以被用户修改的数据文件**（`rules.yaml`），而护栏必须
//! 永远有效。它是「即使前面所有判断都错了、即使规则库被改坏了，
//! 也不会出事」的保证。所以这里的每一条都是**硬编码的系统关键路径**。
//!
//! 两层配合，职责分明：
//! - **护栏（本模块）** —— 挡死系统关键路径。不看规则库，不会被绕过。
//! - **规则库判定（上层 CLI）** —— 挡 `protected` / `system-managed` 级内容。
//!   它知道 `node_modules` 在项目里可删、在软件安装目录里不可删，护栏做不到这么细。
//!
//! 两者是「与」的关系：任一拒绝，就不执行。

use std::path::{Component, Path, PathBuf};

/// 暂存区目录名。放在每个卷的根目录下。
pub const TRASH_DIR_NAME: &str = ".diskdoctor-trash";

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("路径不存在: {0}")]
    NotFound(String),

    #[error("拒绝操作卷根目录本身: {0}\n盘符根代表整个分区，不能整体移动。")]
    VolumeRoot(String),

    #[error("受系统保护，永不触碰: {0}\n原因：{1}")]
    Protected(String, String),

    #[error("位于暂存区内部，不能操作: {0}")]
    InsideTrash(String),

    #[error("路径解析失败: {0}")]
    Resolve(String),
}

/// 卷根之下、第一层就受保护的目录。
///
/// 这一层是「一刀切」的：其下所有内容都不允许通过暂存区移动。
/// 需要清理其中某一部分时，必须走官方命令（DISM / cleanmgr / 卸载程序）。
const PROTECTED_TOP_DIRS: &[(&str, &str)] = &[
    (
        "windows",
        "操作系统本体。删除会导致无法开机、无法更新，或 SFC 修复失效。",
    ),
    (
        "program files",
        "已安装的软件本体。其中的 node_modules 之类是软件自带的运行时依赖，删了软件直接打不开。请通过「设置 → 应用」卸载。",
    ),
    (
        "program files (x86)",
        "已安装的 32 位软件本体。请通过「设置 → 应用」卸载。",
    ),
    (
        "system volume information",
        "系统还原点与卷影副本的数据，由卷影复制服务独占管理。清理请用「磁盘清理 → 清理系统文件」。",
    ),
    (
        "$recycle.bin",
        "回收站。清空请用系统自带的清空功能，手工搬走会破坏回收站的索引结构。",
    ),
    (
        "$sysreset",
        "系统重置的中间数据，由系统管理。",
    ),
    (
        "recovery",
        "系统恢复环境。删除后无法使用系统修复与重置功能。",
    ),
    (
        "$windows.~bt",
        "系统升级的临时文件，请用磁盘清理（cleanmgr）处理。",
    ),
    (
        "$windows.~ws",
        "系统升级的暂存文件，请用磁盘清理（cleanmgr）处理。",
    ),
];

/// 卷根下的受保护文件（分页/休眠等由系统独占的文件）。
const PROTECTED_ROOT_FILES: &[&str] = &[
    "hiberfil.sys",
    "pagefile.sys",
    "swapfile.sys",
    "bootmgr",
    "bootnxt",
    "ntldr",
    "boot.ini",
    "dumpstack.log.tmp",
];

/// 用户目录下的注册表配置单元，前缀匹配（`ntuser.dat.LOG1` 等也要挡）。
const PROTECTED_USER_FILE_PREFIXES: &[&str] = &["ntuser.dat", "usrclass.dat"];

/// 对一条路径做完整的安全检查。
///
/// 通过则返回**规范化后的绝对路径**（调用方应使用这个路径，
/// 因为它已经解析了 `..` 与符号链接，避免后续操作被路径技巧绕过）。
pub fn check(path: &Path, trash_root: Option<&Path>) -> Result<PathBuf, GuardError> {
    if !path.exists() {
        return Err(GuardError::NotFound(path.display().to_string()));
    }

    // canonicalize 会解析 .. 和符号链接 —— 护栏必须用解析后的真实路径判断，
    // 否则 `C:\Users\me\..\..\Windows` 这类路径能绕过检查。
    let real = std::fs::canonicalize(path)
        .map_err(|e| GuardError::Resolve(format!("{} ({e})", path.display())))?;
    let real = strip_verbatim(&real);
    let plain = real.to_string_lossy().to_string();

    // 卷根
    if is_volume_root(&real) {
        return Err(GuardError::VolumeRoot(plain));
    }

    // 暂存区自身
    if let Some(tr) = trash_root {
        let tr = strip_verbatim(&std::fs::canonicalize(tr).unwrap_or(tr.to_path_buf()));
        if real == tr || real.starts_with(&tr) {
            return Err(GuardError::InsideTrash(plain));
        }
    }

    // 取「卷根之下」的部分
    let rel = relative_after_root(&real);
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();

    if comps.is_empty() {
        return Err(GuardError::VolumeRoot(plain));
    }

    let first = comps[0].as_str();
    let last = comps.last().map(|s| s.as_str()).unwrap_or("");

    // 第一层受保护目录（Windows / Program Files / …）
    //
    // **注意一个刻意设计**：`program` 这个泛名不在硬护栏里，因为
    // `D:\Program` 这种用户自定义安装目录与 `Program Files` 是两回事，
    // 判它属于「安装目录」需要规则库的语境。护栏只管一刀切肯定错的那些。
    if let Some((_, why)) = PROTECTED_TOP_DIRS.iter().find(|(d, _)| *d == first) {
        return Err(GuardError::Protected(plain, (*why).to_string()));
    }

    // 卷根下的受保护文件
    if comps.len() == 1 && PROTECTED_ROOT_FILES.iter().any(|f| *f == first) {
        return Err(GuardError::Protected(
            plain,
            "由操作系统独占使用。清理休眠文件请执行 `powercfg /h off`，\
             页面文件请在系统属性里调整，都不要手工移动。"
                .to_string(),
        ));
    }

    // 用户注册表配置单元（不管在哪个子目录，按文件名前缀挡）
    if PROTECTED_USER_FILE_PREFIXES
        .iter()
        .any(|p| last.starts_with(p))
    {
        return Err(GuardError::Protected(
            plain,
            "这是用户账户的注册表数据本体。删除会导致无法登录、系统异常。".to_string(),
        ));
    }

    Ok(real)
}

/// 取路径在卷根之下的部分。
fn relative_after_root(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut seen_prefix = false;
    for c in p.components() {
        match c {
            Component::Prefix(_) => {
                seen_prefix = true;
            }
            Component::RootDir => {}
            other => {
                if seen_prefix || out.as_os_str().is_empty() {
                    out.push(other.as_os_str());
                }
            }
        }
    }
    out
}

fn is_volume_root(p: &Path) -> bool {
    relative_after_root(p).as_os_str().is_empty()
}

/// 去掉 Windows 的 `\\?\` verbatim 前缀（UNC 路径除外）。
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.to_ascii_uppercase().starts_with("UNC\\") {
            return PathBuf::from(rest);
        }
    }
    drop(s);
    p.to_path_buf()
}

/// 取路径所在的卷根（`C:\`）。用于决定暂存区放哪个卷 ——
/// **放在同一个卷上，暂存动作才能退化成一次 rename**，而不是跨盘复制几个 GB。
pub fn volume_root_of(p: &Path) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    let s = strip_verbatim(&abs);
    let mut comps = s.components();
    match comps.next() {
        Some(Component::Prefix(pre)) => {
            let mut root = PathBuf::from(pre.as_os_str());
            root.push(std::path::MAIN_SEPARATOR.to_string());
            Some(root)
        }
        _ => None,
    }
}

/// 构造暂存区路径：`<卷根>\.diskdoctor-trash`
pub fn trash_root_for(volume_root: &Path) -> PathBuf {
    let mut p = volume_root.to_path_buf();
    p.push(TRASH_DIR_NAME);
    p
}
