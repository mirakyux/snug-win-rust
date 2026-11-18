//! 手机屏桌面状态机，移植自 C# 类 `SnugWin.Core.PhoneScreen.PhoneDesk`。
//!
//! 按小块移植：
//! - [`model`]：状态与数据模型（C# 文件顶部的记录类型）；
//! - [`services`]：平台接口（C# `PhoneServices.cs` 的四个接口与辅助类型）；
//! - `lane`：串行队列（C# `SerialLane`）；
//! - 后续：桌面本体与各个动作。

pub mod lane;
pub mod model;
pub mod services;

pub use model::{
    DeskGone, DeskNotice, DeskState, DeskWork, DeskWorkKind, PhoneScreenPhase, PlacedWindow,
};
pub use services::{
    AppControl, CancelToken, PhoneDisplays, PhoneScreenErrorKind, PhoneScreenException,
    SystemTimeline, Timeline, WindowControl, WindowLook,
};
