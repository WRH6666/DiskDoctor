//! `diskdoctor explain <路径>` —— 把一条路径的归因链路完整摊开。
//!
//! 这个命令解决的问题：用户（或我）看到某个目录被标成"可安全回收"，
//! 但不知道**为什么**。而判断错一次就意味着用户可能删掉不该删的东西，
//! 所以「说清理由」和「判断正确」同样重要。
//!
//! 它输出的不是结论，而是**推理过程**：从卷根一路往下，每一层命中了
//! 哪条规则、优先级多少、下一层是继承还是覆盖。规则库的调试和纠错
//! 全靠它。
//!
//! 纯字符串分析，不读文件系统 —— 所以可以查任何路径，包括不存在的。

use dd_core::Safety;
use dd_rules::Ruleset;

/// 把用户输入的路径转成「去盘符的小写路径」，与扫描器内部的
/// `Snapshot::lower_dir_path` 语义一致。
///
/// 之所以要对齐这个语义：explain 的结果必须和真实扫描一致。
/// 扫描器把路径规则对着 `root_prefix + 相对路径` 求值，而
/// `root_prefix + 相对路径` 恰好等于完整路径去掉盘符 ——
/// 所以无论用户扫的是整个盘还是某个子目录，这里算出来的字符串都一样。
pub fn to_rel_lower(p: &str) -> String {
    let mut s = p.replace('/', "\\");
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        s = s[2..].to_string();
    }
    // 去掉 UNC 前缀 `\\?\` 与 `\\server\share` 的开头多余反斜杠
    let s = s.trim_matches('\\');
    s.to_ascii_lowercase()
}

struct Layer {
    /// 显示用的名字（保留原始大小写）
    name: String,
    /// 累积到这一层的路径（小写）
    path_lower: String,
    /// 这一层自己命中的规则下标
    own: Option<usize>,
    /// 实际生效的规则下标（考虑继承后）
    effective: Option<usize>,
    /// 生效来源是继承吗
    inherited: bool,
    /// 本来名字能命中、但因语境约束被挡掉的规则 id
    suppressed: Vec<String>,
}

/// 对单条路径做完整归因（含从上级目录继承），返回**最终生效**的规则下标。
///
/// 直接转调 dd-rules 里的权威实现 —— 保证「解释」与「执行校验」用的是
/// 同一份逻辑。曾经各写一份，结果同一路径给出两个答案。
pub fn resolve_final(path: &str, rules: &Ruleset, is_dir: bool) -> Option<usize> {
    rules.attribute_path(path, is_dir)
}

pub fn run(target: &str, rules: &Ruleset, is_dir: bool) -> anyhow::Result<()> {
    let rel = to_rel_lower(target);
    if rel.is_empty() {
        anyhow::bail!("请给出一个具体路径，而不是盘符根");
    }

    // 按原始大小写拆出层级，用于显示
    let display_parts: Vec<String> = {
        let cleaned = target.replace('/', "\\");
        let cleaned = if cleaned.len() >= 2 && cleaned.as_bytes()[1] == b':' {
            &cleaned[2..]
        } else {
            cleaned.as_str()
        };
        cleaned
            .trim_matches('\\')
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    };
    let lower_parts: Vec<String> = rel
        .split('\\')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    if display_parts.len() != lower_parts.len() {
        anyhow::bail!("路径解析失败，请用绝对路径（如 C:\\Users\\xxx\\...）");
    }

    let mut layers: Vec<Layer> = Vec::with_capacity(lower_parts.len());
    let mut acc = String::new();
    let mut inherit: Option<usize> = None;

    let n = lower_parts.len();
    for i in 0..n {
        let name = display_parts[i].clone();
        let lower = lower_parts[i].clone();
        acc = if acc.is_empty() {
            lower.clone()
        } else {
            format!("{acc}\\{lower}")
        };

        // 最后一层如果不是目录，走文件判定
        let is_last = i + 1 == n;
        let treat_as_dir = !is_last || is_dir;

        let own = if treat_as_dir {
            rules.attribute_dir(&name, Some(&acc))
        } else {
            rules.attribute_file(&name, Some(&acc))
        };

        // 诊断用：这一层自身「本来能命中」但被语境约束挡掉的规则。
        // 这类情况最隐蔽 —— 用户看到"未命中"，却不知道是因为规则主动退让了。
        let suppressed: Vec<String> = rules
            .all()
            .iter()
            .filter(|r| r.has_context_constraint())
            .filter(|r| {
                r.match_ignoring_context(&name, Some(&acc)) && !r.applies_here(Some(&acc))
            })
            .map(|r| r.id.clone())
            .collect();

        // 与 dd-analyze 的继承逻辑保持一致：取 priority 更高者，
        // 平手时用这一层自己的规则
        let (effective, inherited) = match (own, inherit) {
            (Some(o), None) => (Some(o), false),
            (None, Some(p)) => (Some(p), true),
            (None, None) => (None, false),
            (Some(o), Some(p)) => {
                if rules.rule(o).priority >= rules.rule(p).priority {
                    (Some(o), false)
                } else {
                    (Some(p), true)
                }
            }
        };

        layers.push(Layer {
            name,
            path_lower: acc.clone(),
            own,
            effective,
            inherited,
            suppressed,
        });
        inherit = effective;
    }

    // —— 渲染 ——
    println!("事件归因分析");
    println!("目标: {}", target);
    println!("类型: {}", if is_dir { "目录" } else { "文件" });
    println!("规则库: {} 条", rules.len());
    println!();

    println!("逐层判定（每层的结论会被其下级继承）");
    println!("{}", "─".repeat(96));
    println!(
        "  {:<34} {:<24} {:<12} {:>6}",
        "层级", "命中规则", "安全等级", "优先级"
    );
    println!("{}", "─".repeat(96));

    for l in &layers {
        let indent = "  ".repeat(0);
        let name = if l.name.chars().count() > 32 {
            let c: Vec<char> = l.name.chars().collect();
            format!("{}…", c[..31].iter().collect::<String>())
        } else {
            l.name.clone()
        };

        let (rule_col, safety_col, prio_col) = match (l.own, l.effective) {
            (Some(o), Some(e)) if o == e => {
                let r = rules.rule(o);
                (
                    format!("{}", r.id),
                    r.safety.label().to_string(),
                    r.priority.to_string(),
                )
            }
            (Some(_o), Some(e)) => {
                // 自己命中了但被继承的高优先级规则压住
                let r = rules.rule(e);
                (
                    format!("{}（被上级压住）", rules.rule(l.own.unwrap()).id),
                    r.safety.label().to_string(),
                    r.priority.to_string(),
                )
            }
            (None, Some(e)) => {
                let r = rules.rule(e);
                (
                    "（继承自上级）".to_string(),
                    r.safety.label().to_string(),
                    r.priority.to_string(),
                )
            }
            (Some(o), None) => {
                let r = rules.rule(o);
                (
                    r.id.clone(),
                    r.safety.label().to_string(),
                    r.priority.to_string(),
                )
            }
            (None, None) => ("（未命中）".to_string(), "—".to_string(), "—".to_string()),
        };

        println!(
            "{indent}{:<34} {:<24} {:<12} {:>6}",
            name, rule_col, safety_col, prio_col
        );

        // 被语境约束挡掉的规则必须显式说明，否则用户看到"未命中"
        // 会以为是规则库漏了，而实际上是规则主动退让（这正是安全设计生效了）
        for s in &l.suppressed {
            let r = rules.all().iter().find(|x| &x.id == s);
            let safety = r.map(|x| x.safety.label()).unwrap_or("—");
            println!(
                "{indent}   └─ 规则 `{s}`（本可判为「{safety}」）因语境约束**主动不适用**"
            );
        }
    }
    println!("{}", "─".repeat(96));
    println!();

    // 关键层级：自己命中、且没被继承压住的那些（即"归因区域的根"）
    let roots: Vec<&Layer> = layers
        .iter()
        .filter(|l| l.own.is_some() && !l.inherited)
        .collect();

    let final_layer = layers.last().unwrap();
    match final_layer.effective {
        None => {
            println!("最终结论");
            println!("{}", "─".repeat(96));
            println!("  **未识别** —— 规则库没有覆盖这条路径。");
            println!();
            println!("  这意味着工具无法判断它能不能删。这不是「安全」，");
            println!("  而是「不知道」。需要人工确认，或往规则库里补一条规则。");
            if !roots.is_empty() {
                println!();
                println!("  注：中途有 {} 层命中过规则，但都被后续更具体的判定覆盖了。", roots.len());
            }
        }
        Some(e) => {
            let r = rules.rule(e);
            println!("最终结论");
            println!("{}", "─".repeat(96));
            println!("  归因规则 : {} （{}）", r.id, r.name);
            println!("  安全等级 : {} （可删评分 {}）", r.safety.label(), r.safety.score());
            println!("  匹配方式 : {}", if final_layer.inherited {
                format!(
                    "继承自 `{}`",
                    roots
                        .iter()
                        .rev()
                        .find(|l| l.effective == Some(e))
                        .map(|l| l.path_lower.clone())
                        .unwrap_or_else(|| "上级".into())
                )
            } else {
                "这条路径自己命中".to_string()
            });
            println!("  处置方式 : {}", action_of(r));
            println!();
            println!("  为什么：{}", r.why);
            if !r.recovery.is_empty() {
                println!("  恢复：{}", r.recovery);
            }

            // 判断链上是否有"竞争"—— 也就是中途有别的规则想接管但被压住。
            // 这是最容易出错的地方：优先级设错了，归因就会落到错的规则上。
            let contested: Vec<&Layer> = layers
                .iter()
                .filter(|l| l.own.is_some() && l.effective != l.own)
                .collect();
            if !contested.is_empty() {
                println!();
                println!("  ⚠ 判定过程中有规则被压住，这里最容易出问题：");
                for l in contested {
                    let o = rules.rule(l.own.unwrap());
                    let w = rules.rule(l.effective.unwrap());
                    println!(
                        "    - `{}` 命中 {}（优先级 {}），但被 {}（优先级 {}）压住",
                        l.path_lower, o.id, o.priority, w.id, w.priority
                    );
                }
            }

            if r.safety == Safety::Safe {
                println!();
                println!("  → 这条会被 `clean` 命令列入清理清单。");
            } else {
                println!();
                println!("  → 这条**不会**被 `clean` 命令清理（只处理「可安全回收」档）。");
            }
        }
    }

    Ok(())
}

fn action_of(r: &dd_rules::Rule) -> String {
    match &r.action {
        dd_core::Action::Delete => "清理（移入暂存区，可撤销）".into(),
        dd_core::Action::Command { cmd } => format!("执行官方命令：{cmd}"),
        dd_core::Action::Compact => "压缩虚拟磁盘（不丢数据）".into(),
        dd_core::Action::Review => "需要人工确认".into(),
        dd_core::Action::None => "不做处理".into(),
    }
}
