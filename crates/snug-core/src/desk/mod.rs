//! 手机屏桌面状态机，移植自 C# 类 `SnugWin.Core.PhoneScreen.PhoneDesk`。
//!
//! 按小块移植：
//! - [`model`]：状态与数据模型（C# 文件顶部的记录类型）；
//! - 后续：平台接口、串行队列、各个动作。

pub mod model;

pub use model::{
    DeskGone, DeskNotice, DeskState, DeskWork, DeskWorkKind, PhoneScreenPhase, PlacedWindow,
};
