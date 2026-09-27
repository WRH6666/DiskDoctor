//! 执行层：把「建议」变成「可撤销的动作」。
//!
//! 在此之前，DiskDoctor 只能**看**。这个 crate 给了它**动手**的能力，
//! 而且动手的方式是可逆的 —— 清理 = 移进暂存区，后悔了就移回来。
//!
//! 两个模块分工：
//! - [`guard`] —— 硬编码的安全护栏，不依赖任何可被修改的数据文件
//! - [`trash`] —— 暂存区本体：两阶段提交的移动、恢复、彻底清除
//! - [`space`] —— 卷可用空间实测，让"释放了多少"是可验证的数字而非声称
//!
//! **本 crate 不含任何直接删除用户数据的能力**，唯一不可逆的操作是
//! [`trash::Trash::purge`]，它只能作用于已经在暂存区里的内容。

pub mod guard;
pub mod space;
pub mod trash;

pub use guard::{GuardError, TRASH_DIR_NAME};
pub use space::{available_bytes, DeltaVerdict, SpaceDelta};
pub use trash::{
    StageOutcome, StageRequest, Trash, TrashEntry, TrashError, TrashItem, TrashStatus,
};
