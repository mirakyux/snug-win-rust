//! COM 初始化。给 GUI 主线程用：`IShellItemImageFactory` 这类 shell COM 接口在
//! 没初始化的线程上会直接返回 `CO_E_NOTINITIALIZED`，而 eframe/winit 不会替我们初始化。
//!
//! 规则（容易踩）：`CoInitializeEx` 返回 `S_FALSE` 表示**这个线程早就初始化过**，
//! 这时不能再配对 `CoUninitialize`——多减一次会把别处的 COM 引用计数拆掉。
//! 所以这里把"是不是我们初始化的"作为返回值交给调用方保存。

use windows::Win32::Foundation::S_OK;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

/// 把当前线程初始化为单线程套间（STA）。
///
/// 返回 `true` 表示**这次调用完成了初始化**，退出时必须配对 [`uninit`]；
/// 返回 `false` 表示线程早已初始化（`S_FALSE`）或初始化失败，**不要**调 [`uninit`]。
pub fn init_sta() -> bool {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) == S_OK }
}

/// 与返回 `true` 的 [`init_sta`] 配对释放。
pub fn uninit() {
    unsafe { CoUninitialize() };
}
