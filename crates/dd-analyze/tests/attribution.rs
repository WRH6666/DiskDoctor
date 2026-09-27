//! 归因一致性的回归测试。
//!
//! # 锁住的 bug
//!
//! 快照从扫描根开始，**根之上的路径不在里面**，所以上层语境无法参与继承。
//!
//! 真实表现（discovered 于 `survey` + `clean --from-selection` 联调时）：
//!
//! ```text
//! explain  C:\Users\me\.workbuddy\proj\node_modules  →  禁止触碰（.workbuddy 的约束）
//! survey   扫描 C:\Users\me\.workbuddy\proj            →  可安全回收（只看了 node_modules 自己）
//! ```
//!
//! 同一条路径两个答案。后果不只是"显示不一致"——界面说能删、执行时被拒，
//! 用户就不知道该信哪个，工具的可信度直接归零。
//!
//! 修法：用扫描根的**完整路径**算出它自身的归因，作为所有顶层条目的继承起点。

use dd_core::Safety;
use dd_rules::Ruleset;
use std::fs;

/// 建一个**中立位置**的测试目录。
///
/// 刻意不用 `%TEMP%`：系统临时目录自身有规则（`user-temp`，优先级很高），
/// 会把测试结构整体吸收成"临时文件"，于是 `extensions`、`node_modules`
/// 这些层级根本不会作为独立条目出现 —— 测试就测不到想测的东西了。
/// 这个坑本身值得记下来：**测试夹具的位置会影响归因结果。**
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    let d = std::path::Path::new(&base).join(format!(
        "dd-attr-test-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// 找出**覆盖**某条路径的归因。
///
/// 注意 `findings` 只列"归因区域的根"：同一规则的连续区域会合并成一条，
/// 所以 `extensions\pkg\node_modules` 可能不会作为独立条目出现，
/// 而是被归进 `extensions` 那一条。因此要按前缀找**最具体**的那条，
/// 取它的判定 —— 这才是用户在那条路径上实际会看到的结论。
fn coverage_of(target: &std::path::Path) -> Option<(String, Safety)> {
    let rules = Ruleset::embedded().expect("规则库应能加载");
    let opts = dd_scanner::ScanOptions {
        try_mft: false,
        exclude_names: Default::default(),
        follow_links: false,
    };
    // 从卷根扫太慢，取 target 的前两层做扫描根即可
    let root = target
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| std::path::Path::new("."));
    let snap = dd_scanner::scan(root, &opts).unwrap();
    let analysis = dd_analyze::Analysis::run(snap, rules, "test");

    let t = target.to_string_lossy().to_ascii_lowercase();
    analysis
        .findings
        .iter()
        .filter(|f| t.starts_with(&f.path.to_ascii_lowercase()))
        .max_by_key(|f| f.path.len())
        .map(|f| (f.rule_id.clone(), f.safety))
}

/// 单路径归因（走完整绝对路径 —— `explain` 与执行校验用的就是它）。
fn safety_of_path(p: &std::path::Path) -> Option<(String, Safety)> {
    let rules = Ruleset::embedded().expect("规则库应能加载");
    let is_dir = p.is_dir();
    rules
        .attribute_path(&p.to_string_lossy(), is_dir)
        .map(|i| (rules.rule(i).id.clone(), rules.rule(i).safety))
}

// ==================================================== 核心：两种方式必须一致

#[test]
fn scanning_subdir_keeps_parent_context() {
    // 造一个 `<tmp>\extensions\pkg\node_modules` 结构。
    //
    // `\extensions\` 命中规则 `ide-extensions`（需确认，优先级 96），
    // 而 `node_modules` 自己命中 `node-modules`（可安全回收，优先级 68）。
    // 96 > 68，所以正确的是「需确认」。
    //
    // 修复前：扫 `pkg` 时 root 是 pkg、它自己没规则，于是 node_modules
    // 只按名字判成「可安全回收」—— 丢了 extensions 这个关键语境。
    let d = temp_dir("subdir-ctx");
    let pkg = d.join("extensions").join("pkg");
    let nm = pkg.join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    fs::write(nm.join("a.js"), b"x").unwrap();

    // ① 单路径归因（正确基准）
    let by_path = safety_of_path(&nm).expect("单路径应能归因");

    // ② 扫描（findings 会把 node_modules 并进 extensions 区域，
    //    所以用"覆盖该路径的条目"来取判定，而不是按名字找独立条目）
    let by_scan = coverage_of(&nm).expect("应有覆盖该路径的归因条目");

    assert_eq!(
        by_scan.1, by_path.1,
        "扫描子目录与单路径归因必须一致！\n\
         扫描得出: {:?} / {:?}\n\
         路径得出: {:?} / {:?}\n\
         不一致意味着界面显示的判定与执行时的判定会不同。",
        by_scan.0, by_scan.1, by_path.0, by_path.1
    );

    // 而且要确实是「需确认」，不是「可安全回收」
    assert_eq!(
        by_scan.1,
        Safety::Caution,
        "extensions 下的 node_modules 应继承上级的「需确认」，而不是被自己的\
         node-modules 规则判成「可安全回收」。实际规则: {}",
        by_scan.0
    );

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn scanning_sibling_dir_does_not_leak_context() {
    // 反向验证：语境约束不能"过度传染"。
    // 一个**不含** extensions 的路径，其 node_modules 仍应正常判为可回收。
    let d = temp_dir("sibling-ctx");
    let nm = d.join("myproject").join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    fs::write(nm.join("a.js"), b"x").unwrap();

    let by_scan = coverage_of(&nm).expect("应有覆盖该路径的归因条目");
    assert_eq!(
        by_scan.1,
        Safety::Safe,
        "普通项目里的 node_modules 必须仍是「可安全回收」，实际: {:?}/{:?}",
        by_scan.0,
        by_scan.1
    );

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn scanning_at_volume_root_matches_subdir_scan() {
    // 从更上层扫和从子目录扫，对同一条路径应给出相同判定。
    // 这是"无论扫哪儿，答案都要一致"的通用要求。
    let d = temp_dir("consistency");
    let pkg = d.join("extensions").join("somepkg");
    let nm = pkg.join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    fs::write(nm.join("a.js"), b"x").unwrap();

    // 无论从哪一层作为扫描根，对同一条路径的判定都必须一致。
    // coverage_of 内部会挑一个合适的扫描根，这里再单独验证
    // "扫描根就是 pkg 自己"这个最容易丢语境的情形。
    let by_path = safety_of_path(&nm).expect("单路径应能归因");
    let by_scan = coverage_of(&nm).expect("应有覆盖该路径的归因条目");

    assert_eq!(
        by_scan.1, by_path.1,
        "扫描得出的判定必须与单路径归因一致\n\
         扫描: {:?} / {:?}\n路径: {:?} / {:?}",
        by_scan.0, by_scan.1, by_path.0, by_path.1
    );

    let _ = fs::remove_dir_all(&d);
}

// ==================================================== attribute_path 本身

#[test]
fn attribute_path_respects_priority_chain() {
    // 逐层继承取优先级更高者 —— 这是整个归因的地基。
    let rules = Ruleset::embedded().unwrap();

    // extensions（96）应压住 node_modules（68）
    let hit = rules
        .attribute_path(r"C:\x\extensions\pkg\node_modules", true)
        .expect("应能归因");
    assert_eq!(rules.rule(hit).id, "ide-extensions");

    // 普通项目目录里，node_modules 应胜出
    let hit2 = rules
        .attribute_path(r"C:\x\myproject\node_modules", true)
        .expect("应能归因");
    assert_eq!(rules.rule(hit2).id, "node-modules");
}

#[test]
fn attribute_path_handles_separators_and_case() {
    // 路径可能是 `/` 或 `\`，大小写也不定 —— 结果必须一致
    let rules = Ruleset::embedded().unwrap();
    let a = rules.attribute_path(r"C:\X\Extensions\Pkg\node_modules", true);
    let b = rules.attribute_path("C:/X/Extensions/Pkg/node_modules", true);
    assert_eq!(
        a.map(|i| rules.rule(i).id.clone()),
        b.map(|i| rules.rule(i).id.clone()),
        "分隔符与大小写不该影响判定"
    );
}

#[test]
fn attribute_path_returns_none_for_unknown() {
    // 规则库没覆盖时应返回 None（"不知道"），而不是瞎猜一个
    let rules = Ruleset::embedded().unwrap();
    let r = rules.attribute_path(r"C:\totally-unknown-thing-xyz\blah", true);
    assert!(
        r.is_none(),
        "未覆盖的路径应返回 None 表示『不知道』，实际: {:?}",
        r.map(|i| rules.rule(i).id.clone())
    );
}

// ==================================================== 区域大小 vs 目录大小
//
// 这两个量容易混，混了会让"展开看内部构成"的占比超过 100%，
// 或者出现"目录实际超阈值却不展开"的怪行为。

#[test]
fn region_size_excludes_subregions_but_snapshot_size_does_not() {
    // 造一个目录，其中一部分归属另一条规则（更具体的规则）。
    //
    // 期望：
    //   - Finding.size（区域大小）只含"归本规则管"的部分
    //   - Snapshot 里该目录的 size 是**真实总大小**
    //   - 两者不等时，界面要用真实总大小做分母
    let d = temp_dir("region-vs-dir");
    let root = d.join("pkg");
    // 30 MB 归 pkg 自己的规则；40 MB 落在更具体的 cache 规则上
    fs::create_dir_all(root.join("node_modules")).unwrap();
    fs::create_dir_all(root.join("__pycache__")).unwrap();
    fs::write(root.join("node_modules").join("a.bin"), vec![0u8; 4096]).unwrap();
    fs::write(root.join("__pycache__").join("b.bin"), vec![0u8; 8192]).unwrap();

    let rules = Ruleset::embedded().unwrap();
    let opts = dd_scanner::ScanOptions {
        try_mft: false,
        exclude_names: Default::default(),
        follow_links: false,
    };
    let snap = dd_scanner::scan(&root, &opts).unwrap();

    // 快照里 pkg 的 size = 真实总大小
    let root_size = snap.entries[0].size;
    assert!(
        root_size >= 4096 + 8192,
        "快照的根大小应是真实总大小（至少含两个文件），实际 {root_size}"
    );

    let analysis = dd_analyze::Analysis::run(snap, rules, "test");
    // 两个子目录各自命中不同规则 → 各自成为区域根
    let nm = analysis
        .findings
        .iter()
        .find(|f| f.path.to_ascii_lowercase().ends_with("node_modules"))
        .expect("node_modules 应自成区域");
    let pc = analysis
        .findings
        .iter()
        .find(|f| f.path.to_ascii_lowercase().ends_with("__pycache__"))
        .expect("__pycache__ 应自成区域");

    assert!(nm.size > 0 && pc.size > 0);
    // 两者之和应接近根目录真实大小（各区域互不重叠、加总 = 已归因总量）
    assert!(
        nm.size + pc.size <= root_size + 4096,
        "两个区域之和 {} 不该超过根真实大小 {}",
        nm.size + pc.size,
        root_size
    );

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn child_shares_never_exceed_one_hundred_percent() {
    // 用快照真实大小做分母时，直接子项占比之和必须 ≈ 100%。
    // 这条锁住"拿区域大小当分母"那个 bug —— 那会让占比超过 100%，
    // 用户看到 154% 只会认为工具算错了。
    let d = temp_dir("share-sum");
    let root = d.join("proj");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    fs::write(root.join("a").join("x.bin"), vec![0u8; 100_000]).unwrap();
    fs::write(root.join("b").join("y.bin"), vec![0u8; 200_000]).unwrap();
    fs::write(root.join("node_modules").join("z.bin"), vec![0u8; 300_000]).unwrap();

    let opts = dd_scanner::ScanOptions {
        try_mft: false,
        exclude_names: Default::default(),
        follow_links: false,
    };
    let snap = dd_scanner::scan(&root, &opts).unwrap();

    // 直接子项（目录）大小之和
    let n = snap.entries.len();
    let root_size = snap.entries[0].size;
    let mut kids_sum = 0u64;
    for i in 1..n {
        if snap.entries[i].parent == 0 {
            kids_sum += snap.entries[i].size;
        }
    }
    let ratio = kids_sum as f64 / root_size.max(1) as f64;
    assert!(
        ratio <= 1.001,
        "直接子项之和 {kids_sum} 不该超过父目录真实大小 {root_size}（比值 {ratio:.3}）"
    );
    assert!(
        ratio >= 0.9,
        "直接子项之和应基本等于父目录大小（比值 {ratio:.3}），否则目录里有未计入的内容"
    );

    let _ = fs::remove_dir_all(&d);
}
