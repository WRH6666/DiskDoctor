//! 归因规则引擎。
//!
//! 这是产品的"知识层"：把一堆冷冰冰的路径翻译成
//! 「这是什么 · 能不能删 · 删了会怎样 · 该怎么删」。
//!
//! 规则按文件中的书写顺序求值，**首次命中即停止**，因此规则库必须
//! 由具体到宽泛排列（`appdata\local\npm-cache` 必须排在 `appdata` 之前）。
//! 可用 `priority` 字段打断书写顺序，数值大的先算。

use dd_core::{Action, Category, Safety};
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchKind {
    /// 名字精确匹配，忽略大小写
    Name,
    /// 名字通配符匹配（`*` `?`），忽略大小写
    NameGlob,
    /// 扩展名精确匹配（不含点）
    Ext,
    /// 目录相对路径的结尾（如 `appdata\local\npm-cache`）
    PathSuffix,
    /// 目录相对路径包含子串
    PathContains,
    /// 目录相对路径整体通配（如 `appdata\local\jetbrains\*\caches`）
    PathGlob,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawRule {
    pub id: String,
    pub name: String,
    /// 为什么能删 / 为什么不能删（面向用户的解释）
    #[serde(default)]
    pub why: String,
    /// 删了之后怎么恢复
    #[serde(default)]
    pub recovery: String,
    pub r#match: RawMatch,
    pub category: Category,
    pub safety: Safety,
    #[serde(default)]
    pub action: ActionKind,
    /// 官方命令模板，action 为 command 时必填
    #[serde(default)]
    pub command: Option<String>,
    /// 更高优先，默认 0；同值保持文件顺序
    #[serde(default)]
    pub priority: i32,
    /// **语境约束**：祖先路径中出现这些片段时，本规则不适用。
    ///
    /// 存在的理由是一个真实事故：`node_modules` 规则假设"这是项目里
    /// 可重建的产物"，但 IDE 扩展目录里的 `node_modules`
    /// （如 `.vscode\extensions\ms-python.python-xxx\out\client\node_modules`）
    /// 是**扩展自带的运行时依赖**，删了扩展直接失效。
    ///
    /// 靠调 priority 是压不住的 —— 那要求每新增一个 IDE / 应用目录都记得
    /// 设一个更高的值，迟早漏。用语境约束则是加法式的：
    /// 声明一条"可重建"规则时，顺手说清它在哪些语境下不成立即可。
    #[serde(default)]
    pub unless_below: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RawMatch {
    pub kind: MatchKind,
    pub pattern: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    #[default]
    None,
    Delete,
    Command,
    Compact,
    Review,
}

impl RawRule {
    fn to_action(&self) -> Action {
        match self.action {
            ActionKind::None => Action::None,
            ActionKind::Delete => Action::Delete,
            ActionKind::Compact => Action::Compact,
            ActionKind::Review => Action::Review,
            ActionKind::Command => Action::Command {
                cmd: self.command.clone().unwrap_or_default(),
            },
        }
    }
}

/// 已编译的匹配器，避免求值时反复小写化/解析通配符。
#[derive(Debug, Clone)]
enum Matcher {
    Name(String),
    NameGlob(Vec<GlobPart>),
    Ext(String),
    PathSuffix(String),
    PathContains(String),
    PathGlob(Vec<GlobPart>),
}

#[derive(Debug, Clone)]
enum GlobPart {
    Literal(char),
    AnyChar,
    AnySeq,
}

impl Matcher {
    /// 对**原始大小写**的名字求值，内部做大小写不敏感比较。
    ///
    /// 刻意不在调用处统一 `to_lowercase()`：归因要跑在百万级条目上，
    /// 逐条小写化就是上百万次堆分配。直接比较可以做到零分配。
    fn matches_name(&self, name: &str) -> bool {
        match self {
            Matcher::Name(p) => name.eq_ignore_ascii_case(p),
            Matcher::Ext(e) => ext_of(name)
                .map(|x| x.eq_ignore_ascii_case(e))
                .unwrap_or(false),
            Matcher::NameGlob(parts) => glob_match(parts, name),
            // 路径类规则不在名字上求值
            _ => false,
        }
    }

    fn matches_path(&self, path_lower: &str) -> bool {
        match self {
            Matcher::PathSuffix(p) => {
                path_lower == p.as_str() || path_lower.ends_with(&format!("\\{p}"))
            }
            Matcher::PathContains(p) => path_lower.contains(p.as_str()),
            Matcher::PathGlob(parts) => glob_match(parts, path_lower),
            _ => false,
        }
    }

    fn needs_path(&self) -> bool {
        matches!(
            self,
            Matcher::PathSuffix(_) | Matcher::PathContains(_) | Matcher::PathGlob(_)
        )
    }
}

fn ext_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 || dot + 1 >= name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

/// 极简通配符匹配：`*` 任意长度（**可以跨路径分隔符**）、`?` 单字符。
/// 线性回溯实现，大小写不敏感。
fn glob_match(parts: &[GlobPart], text: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_p: Option<usize> = None;
    let mut star_t = 0usize;

    while ti < t.len() {
        if pi < parts.len() {
            match &parts[pi] {
                GlobPart::AnySeq => {
                    star_p = Some(pi);
                    star_t = ti;
                    pi += 1;
                    continue;
                }
                GlobPart::AnyChar => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                GlobPart::Literal(c) => {
                    if c.eq_ignore_ascii_case(&t[ti]) {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
            }
        }
        match star_p {
            Some(sp) => {
                pi = sp + 1;
                star_t += 1;
                ti = star_t;
            }
            None => return false,
        }
    }
    while pi < parts.len() && matches!(parts[pi], GlobPart::AnySeq) {
        pi += 1;
    }
    pi == parts.len()
}

fn compile_glob(pattern: &str) -> Vec<GlobPart> {
    let mut out = Vec::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '*' => {
                if !matches!(out.last(), Some(GlobPart::AnySeq)) {
                    out.push(GlobPart::AnySeq)
                }
            }
            '?' => out.push(GlobPart::AnyChar),
            other => out.push(GlobPart::Literal(other)),
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub why: String,
    pub recovery: String,
    pub category: Category,
    pub safety: Safety,
    pub action: Action,
    pub priority: i32,
    matcher: Matcher,
    /// 已小写化的语境约束片段
    unless_in_path: Vec<String>,
}

impl Rule {
    pub fn is_path_rule(&self) -> bool {
        self.matcher.needs_path()
    }

    /// 是否声明了语境约束。
    pub fn has_context_constraint(&self) -> bool {
        !self.unless_in_path.is_empty()
    }

    /// 本规则在当前路径语境下是否适用。
    ///
    /// 只看**祖先部分**（去掉最后一段），避免规则片段与自身名字撞上。
    pub fn applies_here(&self, path_lower: Option<&str>) -> bool {
        self.applies_in(path_lower)
    }

    /// 忽略语境约束，只测匹配模式本身。**仅供诊断使用**
    /// （`explain` 命令用它回答"这条规则本来能命中吗、为什么没生效"）。
    pub fn match_ignoring_context(&self, name: &str, path_lower: Option<&str>) -> bool {
        if self.is_path_rule() {
            return path_lower
                .map(|p| self.matcher.matches_path(p))
                .unwrap_or(false);
        }
        self.matcher.matches_name(name)
    }

    fn applies_in(&self, path_lower: Option<&str>) -> bool {
        if self.unless_in_path.is_empty() {
            return true;
        }
        let Some(p) = path_lower else {
            // 没给路径信息时保守放行 —— 宁可多判一条，不要让规则静默失效。
            // 调用方通过 `Ruleset::needs_path_context` 保证该给的时候一定给。
            return true;
        };
        // 两种分隔符都要处理：路径可能用 `\` 或 `/`，规则片段里的 `\`
        // 也可能写成 `/`。不统一处理的话，语境约束会**静默失效** ——
        // 那意味着本该被挡住的危险判定悄悄生效。
        let mut ancestor = match p.rfind(['\\', '/']) {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        };
        if ancestor.contains('/') {
            ancestor = ancestor.replace('/', "\\");
        }
        !self
            .unless_in_path
            .iter()
            .any(|frag| ancestor.contains(frag.as_str()))
    }

    /// 对目录求值：先试路径规则，再试名字规则。
    /// `path_lower` 必须是**已小写化**的相对路径。
    pub fn match_dir(&self, name: &str, path_lower: Option<&str>) -> bool {
        if !self.applies_in(path_lower) {
            return false;
        }
        if self.is_path_rule() {
            return path_lower
                .map(|p| self.matcher.matches_path(p))
                .unwrap_or(false);
        }
        self.matcher.matches_name(name)
    }

    /// 对文件求值：只考虑名字与扩展名规则。
    pub fn match_file(&self, name: &str, path_lower: Option<&str>) -> bool {
        if !self.applies_in(path_lower) {
            return false;
        }
        if self.is_path_rule() {
            return false;
        }
        self.matcher.matches_name(name)
    }
}

/// 一条命中结果：规则带来的全部上下文。
#[derive(Debug, Clone)]
pub struct Attribution {
    pub rule_id: Option<String>,
    pub label: Option<String>,
    pub category: Category,
    pub safety: Safety,
    pub action: Action,
    pub why: String,
    pub recovery: String,
    /// true 表示由祖先目录继承而来，不是这条路径本身命中的
    pub inherited: bool,
}

impl Attribution {
    pub fn unknown() -> Self {
        Self {
            rule_id: None,
            label: None,
            category: Category::Unknown,
            safety: Safety::Caution,
            action: Action::Review,
            why: "规则库未覆盖这类路径，无法判断来源，建议人工确认后再处理".into(),
            recovery: String::new(),
            inherited: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("规则文件解析失败: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("存在重复的规则 id: {0}")]
    DuplicateId(String),
    #[error("规则 {0} 声明了 command 动作但没有填 command 字段")]
    MissingCommand(String),
    #[error("规则 {0} 使用了空的匹配模式")]
    EmptyPattern(String),
}
/// 已编译的规则库。
///
/// 实现 `Clone` 是为了让上层（`Analysis`）能持有所有权而不必引入生命周期
/// 参数。规则库只有几百条，克隆一次的开销可以忽略。
#[derive(Debug, Clone)]
pub struct Ruleset {
    rules: Vec<Rule>,
    /// 需要路径信息的规则数（路径规则 + 声明了语境约束的规则）。
    /// 为 0 时可以完全跳过路径拼接，省掉百万次字符串分配。
    rules_needing_path: usize,
}

impl Ruleset {
    /// 从内嵌的规则库加载（随二进制分发，用户无需关心文件位置）。
    pub fn embedded() -> Result<Self, RulesError> {
        Self::from_yaml(include_str!("../rules/rules.yaml"))
    }

    /// 从外部 YAML 加载，便于用户自行扩充。
    pub fn from_yaml(text: &str) -> Result<Self, RulesError> {
        let raw: Vec<RawRule> = serde_yaml::from_str(text)?;
        Self::compile(raw)
    }

    pub fn compile(mut raw: Vec<RawRule>) -> Result<Self, RulesError> {
        let mut seen: HashSet<String> = HashSet::new();
        for r in &raw {
            if !seen.insert(r.id.clone()) {
                return Err(RulesError::DuplicateId(r.id.clone()));
            }
            if r.r#match.pattern.trim().is_empty() {
                return Err(RulesError::EmptyPattern(r.id.clone()));
            }
            if r.action == ActionKind::Command && r.command.is_none() {
                return Err(RulesError::MissingCommand(r.id.clone()));
            }
        }

        // 稳定排序：priority 降序，同 priority 保持文件顺序。
        // Rust 的 sort_by 是稳定排序，满足要求。
        raw.sort_by(|a, b| b.priority.cmp(&a.priority));

        let mut rules = Vec::with_capacity(raw.len());
        for r in raw {
            let p = r.r#match.pattern.to_ascii_lowercase();
            let matcher = match r.r#match.kind {
                MatchKind::Name => Matcher::Name(p),
                MatchKind::NameGlob => Matcher::NameGlob(compile_glob(&p)),
                MatchKind::Ext => Matcher::Ext(p.trim_start_matches('.').to_string()),
                MatchKind::PathSuffix => {
                    Matcher::PathSuffix(p.trim_matches('\\').to_string())
                }
                MatchKind::PathContains => Matcher::PathContains(p),
                MatchKind::PathGlob => {
                    // 规则里写的是相对路径的形态（如 appdata\roaming\code\*cache），
                    // 但实际路径前面还有用户名等层级（users\xxx\appdata\...）。
                    // 所以不在开头补一层通配就无法命中。
                    let mut p = p.replace('/', "\\");
                    if !p.starts_with('*') {
                        p.insert(0, '*');
                    }
                    Matcher::PathGlob(compile_glob(&p))
                }
            };
            // 先把 action 算出来：to_action() 需要借用 r，
            // 而下面构造 Rule 时会按字段把 r 拆开（部分移动）。
            let action = r.to_action();
            rules.push(Rule {
                id: r.id,
                name: r.name,
                why: r.why,
                recovery: r.recovery,
                category: r.category,
                safety: r.safety,
                action,
                priority: r.priority,
                matcher,
                unless_in_path: r
                    .unless_below
                    .iter()
                    .map(|s| s.replace('/', "\\").to_ascii_lowercase())
                    .collect(),
            });
        }
        let rules_needing_path = rules
            .iter()
            .filter(|r| r.is_path_rule() || r.has_context_constraint())
            .count();
        Ok(Self {
            rules,
            rules_needing_path,
        })
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 是否需要为条目准备路径信息。
    ///
    /// 两条路径规则需要它；声明了语境约束（`unless_below`）的**名字/扩展名
    /// 规则**同样需要 —— 否则 `node_modules` 在 IDE 扩展目录下就压不住。
    pub fn needs_path_context(&self) -> bool {
        self.rules_needing_path > 0
    }

    /// 对单个目录求值，返回规则在规则表中的下标。
    /// `path_lower` 在 `needs_path_context()` 为 true 时必须提供。
    pub fn attribute_dir(&self, name: &str, path_lower: Option<&str>) -> Option<usize> {
        self.rules.iter().position(|r| r.match_dir(name, path_lower))
    }

    /// 对单个文件求值，返回规则下标。
    ///
    /// 注意也要传 `path_lower` —— 语境约束对文件同样有效
    /// （例如 `*.log` 在 `WinSxS` 下不应被当作可清理的日志）。
    pub fn attribute_file(&self, name: &str, path_lower: Option<&str>) -> Option<usize> {
        self.rules.iter().position(|r| r.match_file(name, path_lower))
    }

    /// 对**一整条绝对路径**做逐层归因，返回最终生效的规则下标。
    ///
    /// # 为什么要在这个层做
    ///
    /// 归因的正确性依赖"从卷根一路继承下来"：`C:\Users\me\.workbuddy\x\y`
    /// 的判定，可能由中间的 `.workbuddy`（禁止触碰）决定，而不是由 `y`
    /// 自己的名字决定。
    ///
    /// 所以在 dd-rules 这一层提供唯一的权威实现，三个使用方共用：
    /// - `explain` 命令（向用户解释为什么）
    /// - `clean --from-selection`（执行前重新校验）
    /// - `dd-analyze`（算扫描根自身的归因，作为继承起点）
    ///
    /// 分散实现过一次，结果 `survey` 说"可勾选"而 `clean` 说"拒绝" ——
    /// 同一路径两个答案，用户没法信任工具。所以**必须只有一份**。
    pub fn attribute_path(&self, abs_path: &str, is_dir: bool) -> Option<usize> {
        let rel = to_rel_lower(abs_path);
        let parts: Vec<&str> = rel.split('\\').filter(|s| !s.is_empty()).collect();
        let n = parts.len();
        if n == 0 {
            return None;
        }

        let mut acc = String::new();
        let mut best: Option<usize> = None;

        for (i, part) in parts.iter().enumerate() {
            acc = if acc.is_empty() {
                (*part).to_string()
            } else {
                format!("{acc}\\{part}")
            };
            let is_last = i + 1 == n;
            let treat_as_dir = !is_last || is_dir;
            let own = if treat_as_dir {
                self.attribute_dir(part, Some(&acc))
            } else {
                self.attribute_file(part, Some(&acc))
            };
            // 取优先级更高者；平手时用这一层自己的（与 dd-analyze 一致）
            best = match (own, best) {
                (Some(o), None) => Some(o),
                (None, b) => b,
                (Some(o), Some(b)) => {
                    if self.rule(o).priority >= self.rule(b).priority {
                        Some(o)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    }

    /// 按下标取规则。只接受 [`Self::attribute_dir`] / [`Self::attribute_file`] 的返回值。
    pub fn rule(&self, idx: usize) -> &Rule {
        &self.rules[idx]
    }

    pub fn all(&self) -> &[Rule] {
        &self.rules
    }
}

impl Rule {
    pub fn to_attribution(&self, inherited: bool) -> Attribution {
        Attribution {
            rule_id: Some(self.id.clone()),
            label: Some(self.name.clone()),
            category: self.category,
            safety: self.safety,
            action: self.action.clone(),
            why: self.why.clone(),
            recovery: self.recovery.clone(),
            inherited,
        }
    }
}

/// 把绝对路径转成「去掉盘符、全小写、反斜杠分隔」的形式。
///
/// 规则匹配统一用这个形式 —— 它与扫描器内部的 `root_prefix + 相对路径`
/// 完全等价（两者都等于"完整路径去掉盘符"），所以无论扫整个盘还是某个
/// 子目录，同一条路径算出的字符串都一样，判定也就一致。
fn to_rel_lower(p: &str) -> String {
    let s = p.replace('/', "\\");
    let s = if s.len() >= 2 && s.as_bytes()[1] == b':' {
        s[2..].to_string()
    } else {
        s
    };
    // 去掉 UNC 前缀 `\\?\` 或开头的多余反斜杠
    s.trim_matches('\\').to_ascii_lowercase()
}
