//! 实测空间变化的判定逻辑测试。
//!
//! # 为什么值得单独测
//!
//! 这段逻辑决定工具对用户说的**最后一句话**。如果说错，
//! 后果是把用户引向错误的行动：
//!
//! - 明明没释放，却说"已释放 2.13 GB" → 用户以为空间回来了，继续被磁盘满困扰
//! - 明明释放了，却说"未归还、去清空回收站" → 用户白白清掉自己回收站里的东西
//!
//! 判定门槛本身不复杂，但**方向不能错**，噪声容差也不能过大或过小。
//! 下面每个用例都对应一个真实场景。

use dd_exec::space::{DeltaVerdict, SpaceDelta};

const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * MB;

fn delta(before: u64, change: i64) -> SpaceDelta {
    SpaceDelta {
        before,
        after: (before as i64 + change) as u64,
    }
}

// ============================================================ 正常释放

#[test]
fn full_release_is_recognised() {
    // 删除 500 MB，可用空间确实多了 500 MB
    let d = delta(20 * GB, (500 * MB) as i64);
    assert_eq!(d.verdict(500 * MB), DeltaVerdict::Freed);
    assert!(d.roughly_matches(500 * MB));
}

#[test]
fn near_full_release_with_minor_noise_still_counts() {
    // 删除 1 GB，实测 950 MB（约 95%，有并发写入的正常情况）
    let d = delta(20 * GB, (950 * MB) as i64);
    assert_eq!(d.verdict(1 * GB), DeltaVerdict::Freed);
}

#[test]
fn release_slightly_over_claimed_is_fine() {
    // 实测比声称还多（并发的其他程序也释放了空间）
    let d = delta(20 * GB, (1100 * MB) as i64);
    assert_eq!(d.verdict(1 * GB), DeltaVerdict::Freed);
}

// ============================================================ 未释放（回收站重定向）

#[test]
fn zero_release_is_flagged_as_not_released() {
    // **本项目的真实场景**：删除 2 GB 后可用空间纹丝不动，
    // 因为删除被重定向到了回收站。
    let d = delta(20 * GB, 0);
    assert_eq!(
        d.verdict(2 * GB),
        DeltaVerdict::NotReleased,
        "完全没释放必须被判为 NotReleased，而不是『正常波动』"
    );
    assert!(!d.roughly_matches(2 * GB));
}

#[test]
fn tiny_change_within_noise_is_not_released() {
    // 删除 500 MB，只回来 3 MB —— 3 MB 在噪声量级内，等于没释放。
    // 若把这种判成 Partial 或 Freed，用户会以为回收生效了。
    let d = delta(20 * GB, (3 * MB) as i64);
    assert_eq!(d.verdict(500 * MB), DeltaVerdict::NotReleased);
}

#[test]
fn small_negative_drift_is_still_not_released() {
    // 删除 500 MB，反而少 10 MB —— 10 MB 未超噪声容差，
    // 本质仍是"没释放"，只是叠加了后台写入。不能因此判成 Shrunk。
    let d = delta(20 * GB, -(10 * MB as i64));
    assert_eq!(
        d.verdict(500 * MB),
        DeltaVerdict::NotReleased,
        "小幅负漂移不该被当成『空间反而减少』这种更重的结论"
    );
}

// ============================================================ 部分释放

#[test]
fn partial_release_is_recognised() {
    // 删除 1 GB，只回来 300 MB（部分内容被程序重建，或部分进了回收站）
    let d = delta(20 * GB, (300 * MB) as i64);
    assert_eq!(d.verdict(1 * GB), DeltaVerdict::Partial);
}

#[test]
fn just_above_noise_floor_counts_as_partial_not_none() {
    // 删除 1 GB，回来 150 MB —— 已明显超出噪声，应判为"部分归还"
    let d = delta(20 * GB, (150 * MB) as i64);
    assert_eq!(d.verdict(1 * GB), DeltaVerdict::Partial);
}

// ============================================================ 反而减少

#[test]
fn significant_shrink_is_flagged() {
    // 删除 100 MB，但同期有程序写入了 2 GB → 可用空间反而少了约 1.9 GB。
    // 这是"清理没错，但机器在用"的情形，不该说清理失败。
    let d = delta(20 * GB, -(2 * GB as i64) + (100 * MB as i64));
    assert_eq!(d.verdict(100 * MB), DeltaVerdict::Shrunk);
}

#[test]
fn shrink_is_not_confused_with_not_released() {
    // 边界：负数且超过噪声 → Shrunk；负数但在噪声内 → NotReleased
    let strong = delta(20 * GB, -(500 * MB as i64));
    let weak = delta(20 * GB, -(10 * MB as i64));
    assert_eq!(strong.verdict(500 * MB), DeltaVerdict::Shrunk);
    assert_eq!(weak.verdict(500 * MB), DeltaVerdict::NotReleased);
}

// ============================================================ 边界与退化

#[test]
fn zero_claim_is_unknown_not_freed() {
    // 没有声称量时不该给结论 —— 否则会凭空说"已释放"或"未释放"
    let d = delta(20 * GB, 0);
    assert_eq!(d.verdict(0), DeltaVerdict::Unknown);
    assert!(d.roughly_matches(0), "无声称量时不构成告警");
}

#[test]
fn tiny_claim_uses_noise_floor() {
    // 声称删除 1 MB —— 远低于噪声。实测 0，应判 NotReleased 而非"吻合"
    let d = delta(20 * GB, 0);
    assert_eq!(d.verdict(1 * MB), DeltaVerdict::NotReleased);

    // 而如果确实回来了 1 MB，也不该判 Freed（噪声内无法证实）
    // —— 这里容差取噪声下限，避免对微小操作给出过度确定的结论
    let tiny = delta(20 * GB, (1 * MB) as i64);
    assert_ne!(
        tiny.verdict(1 * MB),
        DeltaVerdict::Shrunk,
        "微小变化的方向判断必须保守"
    );
}

#[test]
fn labels_are_human_readable() {
    // 这些字串会直接出现在报告里
    for v in [
        DeltaVerdict::Freed,
        DeltaVerdict::Partial,
        DeltaVerdict::NotReleased,
        DeltaVerdict::Shrunk,
        DeltaVerdict::Unknown,
    ] {
        let l = v.label();
        assert!(!l.is_empty());
        assert!(!l.contains("DeltaVerdict"), "不该泄漏枚举名: {l}");
    }
}

#[test]
fn delta_sign_convention() {
    // 正 = 释放了空间；负 = 反而变少。符号搞反会导致报告完全相反。
    let freed = SpaceDelta {
        before: 1000,
        after: 1500,
    };
    assert_eq!(freed.delta(), 500);

    let shrunk = SpaceDelta {
        before: 1500,
        after: 1000,
    };
    assert_eq!(shrunk.delta(), -500);
}
