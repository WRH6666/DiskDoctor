//! 护栏与暂存区的测试。
//!
//! # 为什么这里的测试格外重要
//!
//! 其他模块出 bug，顶多报告不准、用户被误导。**这两个模块出 bug，
//! 用户会丢数据。** 所以下面的用例不是"覆盖率"，每一条都对应一个
//! 真实会发生的场景 —— 大多数是「如果这里没挡住，会发生什么」。
//!
//! # 关于测试用的临时目录
//!
//! 用到 `C:\` 的用例只做**路径判定**（不实际移动文件），因为护栏的
//! `check()` 需要路径真实存在（它要 canonicalize）。所以这些用例
//! 指向系统里必然存在的目录，但绝不执行 stage。
//!
//! 涉及真实移动的用例全部在 `%TEMP%` 下自建目录，用完即删。

use dd_exec::guard;
use dd_exec::trash::{StageRequest, Trash};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// 把所有测试的暂存区重定向到临时目录。
///
/// **这是必须的。** 不这么做，`cargo test` 会在真实用户的
/// `C:\.diskdoctor-trash` 里留下一堆测试条目 —— 他下次跑
/// `diskdoctor trash list` 就会看到 `dd-test-stage-conflict-...` 这种东西，
/// 而这完全无法解释。测试绝不能污染真实环境。
fn init_test_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join("diskdoctor-test-trash");
        let _ = fs::create_dir_all(&dir);
        std::env::set_var("DISKDOCTOR_TRASH_DIR", &dir);
    });
}

fn temp_dir(tag: &str) -> PathBuf {
    init_test_env();
    let d = std::env::temp_dir().join(format!(
        "dd-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

// ============================================================ 护栏：必须挡住

#[test]
fn guard_blocks_windows_dir() {
    // Windows 目录下的任何东西都不能动
    let p = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
    if !p.exists() {
        return; // 环境不同就跳过
    }
    let err = guard::check(p, None).unwrap_err();
    assert!(
        matches!(err, guard::GuardError::Protected(_, _)),
        "应被拒绝，实际: {err:?}"
    );
    let msg = err.to_string();
    assert!(msg.contains("操作系统本体"), "拒绝理由应说清原因: {msg}");
}

#[test]
fn guard_blocks_program_files() {
    // 软件安装目录：里面可能有名字像 node_modules 的目录，删了软件就废
    let p = Path::new(r"C:\Program Files");
    if !p.exists() {
        return;
    }
    assert!(matches!(
        guard::check(p, None).unwrap_err(),
        guard::GuardError::Protected(_, _)
    ));
}

#[test]
fn guard_blocks_volume_root() {
    // 盘符根代表整个分区，绝不能整体移动
    let p = Path::new(r"C:\");
    assert!(matches!(
        guard::check(p, None).unwrap_err(),
        guard::GuardError::VolumeRoot(_)
    ));
}

#[test]
fn guard_blocks_recycle_bin() {
    // 手工搬走回收站会破坏它的索引结构
    let p = Path::new(r"C:\$Recycle.Bin");
    if !p.exists() {
        return;
    }
    assert!(matches!(
        guard::check(p, None).unwrap_err(),
        guard::GuardError::Protected(_, _)
    ));
}

#[test]
fn guard_blocks_nonexistent() {
    let p = Path::new(r"C:\this-path-should-not-exist-dd-test\file.txt");
    assert!(matches!(
        guard::check(p, None).unwrap_err(),
        guard::GuardError::NotFound(_)
    ));
}

#[test]
fn guard_blocks_ntuser_dat() {
    // 注册表配置单元：删了无法登录
    let p = Path::new(r"C:\Users\Default\NTUSER.DAT");
    if !p.exists() {
        return;
    }
    let err = guard::check(p, None).unwrap_err();
    assert!(
        matches!(err, guard::GuardError::Protected(_, _)),
        "应被拒绝，实际: {err:?}"
    );
    assert!(err.to_string().contains("注册表"), "理由应提到注册表");
}

#[test]
fn guard_blocks_path_traversal_attempt() {
    // `C:\Users\..\..\Windows` 这种路径技巧不能绕过检查
    // canonicalize 会把它解析成真实路径，护栏必须用解析后的结果判断
    let traversing = Path::new(r"C:\Users\..\Windows");
    if !Path::new(r"C:\Windows").exists() {
        return;
    }
    let err = guard::check(traversing, None).unwrap_err();
    assert!(
        matches!(err, guard::GuardError::Protected(_, _)),
        "路径穿越必须被识别为 Windows 目录，实际: {err:?}"
    );
}

#[test]
fn guard_blocks_inside_trash() {
    let d = temp_dir("guard-trash");
    let trash_root = d.join(guard::TRASH_DIR_NAME);
    fs::create_dir_all(trash_root.join("ops")).unwrap();

    let inner = trash_root.join("ops").join("x.txt");
    fs::write(&inner, b"hi").unwrap();

    let err = guard::check(&inner, Some(&trash_root)).unwrap_err();
    assert!(
        matches!(err, guard::GuardError::InsideTrash(_)),
        "暂存区内部的路径不能再次入暂存，实际: {err:?}"
    );

    let _ = fs::remove_dir_all(&d);
}

// ============================================================ 护栏：必须放行

#[test]
fn guard_allows_temp_dir() {
    let d = temp_dir("guard-allow");
    let f = d.join("cache.tmp");
    fs::write(&f, b"x").unwrap();
    assert!(guard::check(&f, None).is_ok(), "临时目录下的文件应可清理");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn guard_allows_custom_program_dir() {
    // `D:\Program` 这类用户自定义安装目录**不在硬护栏里** ——
    // 判它属于「安装目录」需要规则库的语境（护栏只管一刀切肯定错的）。
    // 这里验证的是：护栏不会误伤合法路径，拦它的职责在规则库那层。
    let d = temp_dir("guard-custom");
    let f = d.join("something.dll");
    fs::write(&f, b"x").unwrap();
    assert!(guard::check(&f, None).is_ok());
    let _ = fs::remove_dir_all(&d);
}

// ============================================================ 暂存区：移动与恢复

#[test]
fn stage_moves_file_and_removes_it_from_original_location() {
    let d = temp_dir("stage-basic");
    let f = d.join("a.txt");
    fs::write(&f, b"hello world").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t
        .stage(&[StageRequest::new(&f).reason("测试").rule("test")], "test", false)
        .unwrap();

    assert_eq!(out.entry.status, dd_exec::TrashStatus::Staged);
    assert_eq!(out.entry.items.len(), 1);
    assert!(!f.exists(), "原位置应该已经没有这个文件了");

    // 暂存区里应该有
    let stored = t
        .root()
        .join("ops")
        .join(&out.entry.id)
        .join("data")
        .join(&out.entry.items[0].stored_name);
    assert!(stored.is_file(), "文件应在暂存区内");

    // 清理现场：先把暂存区里的东西删掉，再删测试目录
    t.purge(Some(&out.entry.id), None, false).unwrap();
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn restore_puts_file_back() {
    let d = temp_dir("stage-restore");
    let f = d.join("b.txt");
    fs::write(&f, b"content-to-preserve").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t.stage(&[StageRequest::new(&f)], "test", false).unwrap();
    assert!(!f.exists());

    let r = t.restore(Some(&out.entry.id), false).unwrap();
    assert_eq!(r[0].restored, 1);
    assert!(f.is_file(), "恢复后文件应回到原位置");
    assert_eq!(fs::read(&f).unwrap(), b"content-to-preserve");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn restore_skips_when_original_path_occupied() {
    // 关键安全行为：原位置已经有新文件时，**跳过而不是覆盖**。
    // 覆盖会丢用户的新数据，改名会让用户以为文件丢了。
    let d = temp_dir("stage-conflict");
    let f = d.join("c.txt");
    fs::write(&f, b"original").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t.stage(&[StageRequest::new(&f)], "test", false).unwrap();

    // 用户在清理之后又建了一个同名文件
    fs::write(&f, b"NEW-data-must-survive").unwrap();

    let r = t.restore(Some(&out.entry.id), false).unwrap();
    assert_eq!(r[0].restored, 0, "有冲突时不应恢复");
    assert_eq!(r[0].skipped.len(), 1);
    assert_eq!(
        fs::read(&f).unwrap(),
        b"NEW-data-must-survive",
        "新文件绝不能被覆盖"
    );

    t.purge(Some(&out.entry.id), None, false).unwrap();
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn stage_directory_works() {
    let d = temp_dir("stage-dir");
    let sub = d.join("folder");
    fs::create_dir_all(sub.join("nested")).unwrap();
    fs::write(sub.join("nested").join("x.txt"), b"deep").unwrap();

    let t = Trash::for_path(&sub).unwrap();
    let out = t.stage(&[StageRequest::new(&sub)], "test", false).unwrap();
    assert!(!sub.exists(), "目录应已被移走");

    t.restore(Some(&out.entry.id), false).unwrap();
    assert!(sub.join("nested").join("x.txt").is_file(), "整棵目录应完整恢复");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn stage_rejects_protected_but_processes_others() {
    // 清单里混进一个受保护项时，**不能整批失败** ——
    // 否则用户得重新挑一遍。应跳过它、继续处理其余。
    let d = temp_dir("stage-mixed");
    let ok = d.join("fine.txt");
    fs::write(&ok, b"ok").unwrap();

    let protected = PathBuf::from(r"C:\Windows\System32");
    if !protected.exists() {
        let _ = fs::remove_dir_all(&d);
        return;
    }

    let t = Trash::for_path(&ok).unwrap();
    let out = t
        .stage(
            &[StageRequest::new(&ok), StageRequest::new(&protected)],
            "test",
            false,
        )
        .unwrap();

    assert_eq!(out.entry.items.len(), 1, "合法的那个应被处理");
    assert_eq!(out.rejected.len(), 1, "受保护的那个应被拒绝并报告");
    assert!(!ok.exists());

    t.purge(Some(&out.entry.id), None, false).unwrap();
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn dry_run_changes_nothing() {
    let d = temp_dir("stage-dryrun");
    let f = d.join("keep-me.txt");
    fs::write(&f, b"still here").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t.stage(&[StageRequest::new(&f)], "test", true).unwrap();

    assert!(out.dry_run);
    assert!(f.is_file(), "试运行绝不能动任何文件");
    assert_eq!(fs::read(&f).unwrap(), b"still here");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn purge_actually_frees_and_is_irreversible() {
    let d = temp_dir("stage-purge");
    let f = d.join("purge-me.bin");
    let payload = vec![7u8; 4096];
    fs::write(&f, &payload).unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t.stage(&[StageRequest::new(&f).size(payload.len() as u64)], "test", false)
        .unwrap();

    let stored = t
        .root()
        .join("ops")
        .join(&out.entry.id)
        .join("data")
        .join(&out.entry.items[0].stored_name);
    assert!(stored.is_file());

    let p = t.purge(Some(&out.entry.id), None, false).unwrap();
    assert_eq!(p[0].freed, payload.len() as u64);
    assert!(!stored.exists(), "purge 之后暂存区里的副本应消失");

    // purge 之后不可恢复
    let r = t.restore(Some(&out.entry.id), false);
    assert!(r.is_err() || r.unwrap()[0].restored == 0, "purge 后不该还能恢复");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn manifest_is_durable_and_listable() {
    let d = temp_dir("stage-manifest");
    let f = d.join("listed.txt");
    fs::write(&f, b"abc").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let out = t.stage(&[StageRequest::new(&f).reason("因为可重建").rule("npm-cache")], "我的清理", false)
        .unwrap();

    // 换一个 Trash 实例重新读取，模拟"下次运行程序"
    let t2 = Trash::open(t.volume()).unwrap();
    let list = t2.list(false).unwrap();
    let found = list.iter().find(|e| e.id == out.entry.id).unwrap();

    assert_eq!(found.status, dd_exec::TrashStatus::Staged);
    assert_eq!(found.note, "我的清理");
    assert_eq!(found.items[0].reason, "因为可重建");
    assert_eq!(found.items[0].rule_id, "npm-cache");
    assert!(found.total_size > 0, "应记录了体积");

    t2.purge(Some(&out.entry.id), None, false).unwrap();
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn usage_reflects_staged_bytes() {
    // 注意：`usage()` 是**全卷统计**（这是它该有的语义 —— 用户想知道
    // 整个暂存区占了多少）。但测试是并行跑的，所有测试共用同一个卷的
    // 暂存区（Trash 按卷定位，测试没法各用各的），所以不能用绝对数字断言。
    let d = temp_dir("stage-usage");
    let f = d.join("sized.bin");
    let payload = vec![1u8; 8192];
    fs::write(&f, &payload).unwrap();

    let t = Trash::for_path(&f).unwrap();

    let out = t
        .stage(&[StageRequest::new(&f).size(payload.len() as u64)], "test", false)
        .unwrap();

    // 这一批自己记得对
    assert_eq!(out.entry.total_size, payload.len() as u64);
    assert_eq!(out.entry.remaining(), payload.len() as u64);

    // 全卷清单里能按 id 精确查到自己这条。
    //
    // 这里刻意**不用** usage() 的 before/after 差值来断言：并行跑时别的
    // 测试可能在这两次采样之间清掉了更多字节，差值会失真（曾经就是这样
    // 偶发失败的）。按 id 查与并行无关，验的东西也更准。
    let list = t.list(false).unwrap();
    assert!(
        list.iter().any(|e| e.id == out.entry.id),
        "刚入暂存的条目应出现在全卷清单里"
    );

    // 全卷统计的量级也对：至少把自己这批算进去了
    let after = t.usage().unwrap().0;
    assert!(
        after >= payload.len() as u64,
        "全卷统计（{after} 字节）应至少包含刚入暂存的 {} 字节",
        payload.len()
    );

    t.purge(Some(&out.entry.id), None, false).unwrap();
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn trash_is_on_same_volume_as_target() {
    // 这是「清理瞬时完成、不需要额外空间」的前提。
    // 暂存区必须落在被清理文件所在的卷上，否则 rename 会退化成跨盘复制。
    let d = temp_dir("stage-volume");
    let f = d.join("v.txt");
    fs::write(&f, b"x").unwrap();

    let t = Trash::for_path(&f).unwrap();
    let file_vol = guard::volume_root_of(&f).unwrap();
    assert_eq!(
        t.volume(),
        file_vol.as_path(),
        "暂存区必须与被清理文件同卷"
    );

    let _ = fs::remove_dir_all(&d);
}

// ============================================================ 部分失败：尽力而为

/// 造一个"被独占占用"的目录，让包含它的目录无法被重命名。
///
/// 这是 `AppData\Local\Temp` 的真实处境 —— 它几乎总是被运行中的程序占用。
/// 用 `share_mode(0)`（不共享任何访问）可以让重命名父目录失败。
#[cfg(windows)]
fn make_locked_dir(dir: &Path) -> fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    fs::create_dir_all(dir).unwrap();
    let held = dir.join("held-open.txt");
    fs::write(&held, b"held open by this test").unwrap();
    fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&held)
        .unwrap()
}

#[test]
#[cfg(windows)]
fn one_locked_item_does_not_block_the_rest() {
    // 真实场景复现：Temp 被占用移不动，其余缓存不该被拖累。
    //
    // 修复前的行为是"全有或全无"：一项失败就把已移动的全部搬回去，
    // 于是用户看到「一个新文件都没清掉」，而真正的原因是某个程序占着 Temp。
    // 更糟的是当时 CLI 还会打印"已移入 N 项"——报告虚假成功。
    let d = temp_dir("stage-partial");
    let locked = d.join("locked");
    let _held = make_locked_dir(&locked);

    let fine = d.join("fine");
    fs::create_dir_all(&fine).unwrap();
    fs::write(fine.join("x.txt"), b"x").unwrap();

    let t = Trash::for_path(&d).unwrap();
    let out = t
        .stage(
            &[StageRequest::new(&locked), StageRequest::new(&fine)],
            "test",
            false,
        )
        .unwrap();

    // 没被占用的那个必须移入
    assert!(
        !fine.exists(),
        "未被占用的项应该成功移入暂存区，不该被占用项拖累"
    );
    assert!(out.entry.staged_count() >= 1, "至少应有一项移入成功");
    assert!(out.entry.remaining() > 0, "移入体积应大于 0");

    // 被占用的那个必须还在原处、未被破坏
    assert!(locked.exists(), "被占用的目录必须原封不动");
    assert!(locked.join("held-open.txt").exists());

    // 这一批整体是「可恢复」，不是「已取消」
    assert_eq!(out.entry.status, dd_exec::TrashStatus::Staged);

    // 如果占用确实导致了失败，应记录原因（不留白）
    for it in out.entry.skipped_items() {
        assert!(
            it.skipped_reason.as_ref().map(|s| !s.is_empty()).unwrap_or(false),
            "未移入的项必须记录原因，否则用户无从判断"
        );
    }

    drop(_held);
    // **只清理自己这一批**。测试共享同一个卷的暂存区，
    // 用 `purge(None, ...)` 会连并行跑的其他测试正在用的数据一起清掉 ——
    // 实测就是这么把 restore 的测试搞失败的。
    let _ = t.purge(Some(&out.entry.id), None, false);
    let _ = fs::remove_dir_all(&d);
}

#[test]
#[cfg(windows)]
fn all_items_locked_yields_cancelled_and_touches_nothing() {
    // 当**全部**项目都移不动时，必须：
    //   1. 状态是 Cancelled（不是 Staged）—— 否则报告会说谎
    //   2. 暂存区占用为 0
    //   3. 原文件一个都没动
    let d = temp_dir("stage-alllocked");
    let locked = d.join("locked");
    let _held = make_locked_dir(&locked);

    let t = Trash::for_path(&d).unwrap();
    let out = t
        .stage(&[StageRequest::new(&locked)], "test", false)
        .unwrap();

    assert!(locked.exists(), "移不动的东西必须留在原处");

    // 无论是因为护栏拒绝（canonicalize 失败）还是 rename 失败，
    // 都不能有任何东西进入暂存区
    assert_eq!(out.entry.staged_count(), 0, "不该有任何项被移入");
    assert_eq!(out.entry.remaining(), 0, "占用体积必须是 0");

    // 状态必须明确不是 Staged —— 否则报告会说"已移入 N 项"，而实际一项没动
    assert_ne!(
        out.entry.status,
        dd_exec::TrashStatus::Staged,
        "全部失败时不能报告为 Staged（那是在说谎）"
    );
    assert!(
        !out.entry.status.occupies_space(),
        "全部失败时这批不该占任何空间"
    );

    // 注意：不能用 `usage()` 断言绝对数字 —— 它是**全卷统计**，
    // 并行跑的其他测试也在同一个卷的暂存区里放了东西。
    // 这里只断言"这一批自己没贡献任何占用"。
    assert_eq!(out.entry.remaining(), 0);

    drop(_held);
    let _ = t.clear_history(false);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn remaining_counts_only_actually_staged_items() {
    // 纯数据层验证：未移入的项不能计入"占用了多少"，
    // 否则 `trash usage` 和 `purge` 报告的释放量都会虚高。
    use dd_exec::trash::{TrashEntry, TrashItem, TrashStatus};

    let entry = TrashEntry {
        id: "x".into(),
        created_at: 0,
        volume: r"C:\".into(),
        status: TrashStatus::Staged,
        total_size: 0,
        note: String::new(),
        purged_at: 0,
        items: vec![
            TrashItem {
                seq: 1,
                original_path: r"C:\a".into(),
                stored_name: "0001_a".into(),
                size: 1000,
                is_dir: true,
                reason: String::new(),
                rule_id: String::new(),
                restored: false,
                skipped_reason: None,
            },
            TrashItem {
                seq: 2,
                original_path: r"C:\b".into(),
                stored_name: "0002_b".into(),
                size: 999_999,
                is_dir: true,
                reason: String::new(),
                rule_id: String::new(),
                restored: false,
                skipped_reason: Some("被占用".into()),
            },
        ],
    };

    assert_eq!(entry.staged_count(), 1, "只有一项真的移入了");
    assert_eq!(
        entry.remaining(),
        1000,
        "未移入的 999999 字节不能被算作已占用（否则报告虚高）"
    );
    assert_eq!(entry.skipped_items().count(), 1);
}
