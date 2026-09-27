//! 归因规则引擎的测试，重点是**语境约束**。
//!
//! # 为什么要专门测这个
//!
//! 「可重建的构建产物」这个判断已经出过两次事故，而且每次面目都不同：
//!
//! 1. 第一次：`D:\Program\...\node_modules` — 自定义软件安装目录
//! 2. 第二次：`.vscode\extensions\...\node_modules` — IDE 扩展自带依赖
//!
//! 两次的共同点：`node_modules` 被当成"删了能重装"，而实际是**软件运行的
//! 必备文件**，删了软件直接失效。所以下面既测"该挡的挡住了"，
//! 也测"该放的还放着" —— 只测前者会让规则越收越紧，最后工具没用了。

use dd_core::Safety;
use dd_rules::Ruleset;

fn rules() -> Ruleset {
    Ruleset::embedded().expect("内嵌规则库应能加载")
}

/// 模拟 dd-analyze 的归因链，返回最终胜出的规则 id。
///
/// 必须模拟继承，不能用单层 `attribute_dir` —— 像
/// `myproject\node_modules\react` 这种路径，`react` 这一层自己没有任何规则，
/// 它的归因是**从 `node_modules` 继承**来的。只测单层会得出"未识别"的错误结论，
/// 而用户实际看到的是"构建产物 / 可安全回收"。
fn resolve(rules: &Ruleset, path_lower: &str, is_dir: bool) -> Option<String> {
    let parts: Vec<&str> = path_lower
        .split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .collect();
    let mut acc = String::new();
    let mut best: Option<usize> = None;

    for (i, part) in parts.iter().enumerate() {
        acc = if acc.is_empty() {
            part.to_string()
        } else {
            format!("{acc}\\{part}")
        };
        let is_last = i + 1 == parts.len();
        let own = if !is_last || is_dir {
            rules.attribute_dir(part, Some(&acc))
        } else {
            rules.attribute_file(part, Some(&acc))
        };
        best = match (own, best) {
            (Some(o), None) => Some(o),
            (None, b) => b,
            (Some(o), Some(b)) => {
                if rules.rule(o).priority >= rules.rule(b).priority {
                    Some(o)
                } else {
                    Some(b)
                }
            }
        };
    }

    best.map(|i| rules.rule(i).id.clone())
}

fn safety_resolved(rules: &Ruleset, path_lower: &str) -> Option<Safety> {
    resolve(rules, path_lower, true).map(|id| {
        rules
            .all()
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.safety)
            .unwrap_or(Safety::Caution)
    })
}

// ==================================================== 必须挡住的（两次事故）

#[test]
fn blocks_node_modules_inside_ide_extensions() {
    // 事故 2：IDE 扩展自带的运行时依赖
    let r = rules();
    let cases = [
        r"users\me\.vscode\extensions\ms-python.python-2026.4.0-win32-x64\out\client\node_modules",
        r"users\me\.vscode\extensions\cl.keil-assistant-1.7.0\node_modules",
        r"users\me\.qoder\extensions\ms-python.python-2025.16.0-win32-x64\out\client\node_modules",
        r"users\me\.cursor\extensions\some.ext-1.0\node_modules",
    ];
    for c in cases {
        let id = resolve(&r, c, true);
        assert_ne!(
            id.as_deref(),
            Some("node-modules"),
            "`{c}` 绝不能命中 node-modules（那是扩展的运行时依赖）。实际: {id:?}"
        );
        let safety = safety_resolved(&r, c).expect("应至少命中继承来的规则");
        assert_ne!(
            safety,
            Safety::Safe,
            "`{c}` 绝不能被判为「可安全回收」。实际: {safety:?}"
        );
    }
}

#[test]
fn blocks_node_modules_inside_custom_program_dir() {
    // 事故 1：自定义软件安装目录
    let r = rules();
    let cases = [
        r"program\workbuddy\resources\app.asar.unpacked\node_modules",
        r"program\trae cn\resources\app\node_modules",
        r"program\microsoft vs code\resources\app\extensions\copilot\node_modules",
    ];
    for c in cases {
        let id = resolve(&r, c, true);
        assert_ne!(id.as_deref(), Some("node-modules"), "`{c}` 不该判成可回收");
        assert_ne!(safety_resolved(&r, c), Some(Safety::Safe), "`{c}` 不该是 safe");
    }
}

#[test]
fn blocks_node_modules_inside_bundled_runtimes() {
    // 第三方应用捆绑的运行时
    let r = rules();
    let c = r"appdata\local\someapp\runtimes\cua_node\bin\node_modules";
    assert_ne!(safety_resolved(&r, c), Some(Safety::Safe), "捆绑运行时不能判可回收");
}

#[test]
fn blocks_node_modules_in_user_level_programs() {
    // VS Code / Cursor 的"仅为我安装"位置
    let r = rules();
    let c = r"appdata\local\programs\microsoft vs code\resources\app\node_modules";
    let s = safety_resolved(&r, c);
    assert!(
        matches!(s, Some(Safety::Protected) | Some(Safety::SystemManaged)),
        "用户级安装目录应受保护。实际: {s:?}"
    );
}

#[test]
fn blocks_pycache_inside_site_packages() {
    // 第三方库自带的 pycache —— 删了能重建，但没有意义且可能破坏已装的包
    let r = rules();
    let c = r"appdata\local\programs\someapp\lib\site-packages\foo\__pycache__";
    assert_ne!(safety_resolved(&r, c), Some(Safety::Safe));
}

// ==================================================== 必须放行的（别收太紧）

#[test]
fn still_allows_node_modules_in_user_project() {
    // 这是本工具最核心的价值点，绝不能因为加约束而误伤
    let r = rules();
    let cases = [
        r"myproject\node_modules",
        r"myproject\packages\app\node_modules",
        r"myproject\node_modules\react",
        r"work\some-repo\node_modules",
    ];
    for c in cases {
        let id = resolve(&r, c, true);
        assert_eq!(
            id.as_deref(),
            Some("node-modules"),
            "用户项目里的 node_modules 必须仍然可回收。`{c}` 实际: {id:?}"
        );
        assert_eq!(safety_resolved(&r, c), Some(Safety::Safe));
    }
}

#[test]
fn still_allows_nested_node_modules_in_project() {
    // 嵌套的 node_modules 应跟着外层判定，不该因为"祖先里有 node_modules"而退让
    let r = rules();
    let c = r"myproject\node_modules\a\node_modules\b";
    assert_eq!(safety_resolved(&r, c), Some(Safety::Safe));
}

#[test]
fn still_allows_project_build_artifacts() {
    let r = rules();
    let cases = [
        (r"myproject\src\__pycache__", "pycache"),
        (r"myproject\.pytest_cache", "pytest-cache"),
        (r"myproject\coverage", "coverage"),
        (r"myproject\.next", "nextjs-cache"),
        (r"myproject\.ruff_cache", "ruff-cache"),
    ];
    for (c, expect) in cases {
        let id = resolve(&r, c, true);
        assert_eq!(
            id.as_deref(),
            Some(expect),
            "`{c}` 应仍可回收。实际: {id:?}"
        );
    }
}

// ==================================================== 语境约束机制本身

#[test]
fn context_constraint_only_looks_at_ancestors() {
    // 规则片段若与自身名字相同，不该自己挡住自己。
    // 用一个构造规则验证：unless-below 含 `node_modules`，
    // 那么 `node_modules` 自身所在层不该被它挡住（因为只看祖先部分）。
    let yaml = r#"
- id: nm
  name: Node 依赖
  match: { kind: name, pattern: 'node_modules' }
  category: build-artifact
  safety: safe
  action: delete
  unless-below: ['node_modules']
"#;
    let r = Ruleset::from_yaml(yaml).expect("构造规则库应能加载");

    // 顶层 node_modules：祖先为空 → 适用
    let top = r.attribute_dir("node_modules", Some(r"proj\node_modules"));
    assert!(top.is_some(), "顶层 node_modules 应命中");

    // 嵌套：祖先含 node_modules → 不适用（这正是 unless-below 的设计意图）
    let nested = r.attribute_dir("node_modules", Some(r"proj\node_modules\a\node_modules"));
    assert!(nested.is_none(), "祖先含 node_modules 时应退让");
}

#[test]
fn higher_priority_specific_rule_wins_over_generic_parent() {
    // 验证继承的取舍规则：取 priority 更高者。
    // 场景：应用目录（泛判定为需确认）下的缓存目录（专门判定为可回收）——
    // 具体的缓存判定应该赢，否则所有应用缓存都会被泛化规则吞掉。
    let yaml = r#"
- id: parent-app
  name: 应用数据
  match: { kind: name, pattern: 'myapp' }
  category: app-data
  safety: caution
  action: review
  priority: 40
- id: child-cache
  name: 应用缓存
  match: { kind: name, pattern: 'cache' }
  category: browser-cache
  safety: safe
  action: delete
  priority: 80
"#;
    let r = Ruleset::from_yaml(yaml).expect("构造规则库应能加载");
    assert_eq!(resolve(&r, r"appdata\local\myapp\cache", true).as_deref(), Some("child-cache"));
    // 而应用下面其他没有专门规则的部分，归到应用本身
    assert_eq!(resolve(&r, r"appdata\local\myapp\data", true).as_deref(), Some("parent-app"));
}

#[test]
fn lower_priority_specific_rule_yields_to_stricter_parent() {
    // 反向：具体规则优先级**更低**时，应该让位给更严格的上级。
    // 这正是修 IDE 扩展事故时的目标行为。
    let yaml = r#"
- id: strict-parent
  name: 安装目录
  match: { kind: name, pattern: 'appdir' }
  category: app-data
  safety: protected
  action: none
  priority: 95
- id: loose-child
  name: 看着能删的依赖
  match: { kind: name, pattern: 'node_modules' }
  category: build-artifact
  safety: safe
  action: delete
  priority: 68
"#;
    let r = Ruleset::from_yaml(yaml).expect("构造规则库应能加载");
    assert_eq!(
        resolve(&r, r"appdir\node_modules", true).as_deref(),
        Some("strict-parent"),
        "上级更严格时，具体但宽松的规则必须让位"
    );
}

#[test]
fn unknown_field_is_rejected_not_silently_ignored() {
    // 这是修 `unless-below` 写成 `unless_below` 那次事故的直接产物。
    // serde 遇到不认识的字段默认会**静默**用默认值，于是规则悄悄失效 ——
    // 对一个用来决定"删什么"的规则库，这种沉默是不可接受的。
    let yaml = r#"
- id: t
  name: t
  match: { kind: name, pattern: 'x' }
  category: app-data
  safety: safe
  unless_below: ['a']
"#;
    let err = Ruleset::from_yaml(yaml).expect_err("拼错的字段名必须报错");
    let msg = err.to_string();
    assert!(
        msg.contains("unless_below") || msg.contains("unknown"),
        "错误信息应指出是哪个字段。实际: {msg}"
    );
}

#[test]
fn embedded_ruleset_loads_and_is_sane() {
    let r = rules();
    assert!(r.len() > 200, "规则库规模异常: {}", r.len());
    assert!(r.needs_path_context(), "有语境约束规则时必须准备路径信息");
}

#[test]
fn context_constraint_survives_forward_slash_paths() {
    // 语境约束必须对 `/` 分隔符同样生效。
    // 只认 `\` 的话，调用方一旦传了 `/` 路径，约束会**静默失效** ——
    // 而静默失效在这里意味着本该被挡住的危险判定重新生效。
    let yaml = r#"
- id: nm
  name: Node 依赖
  match: { kind: name, pattern: 'node_modules' }
  category: build-artifact
  safety: safe
  action: delete
  unless-below: ['\extensions\']
"#;
    let r = Ruleset::from_yaml(yaml).expect("构造规则库应能加载");
    let blocked = r.attribute_dir(
        "node_modules",
        Some("users/me/.vscode/extensions/ms-python/out/client/node_modules"),
    );
    assert!(blocked.is_none(), "`/` 分隔符下语境约束也必须生效");
}
