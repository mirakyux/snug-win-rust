//! 当前进程是否以管理员身份运行（C# 版在设置窗里用 `WindowsIdentity`/清单判断）。
//!
//! 只读查询：打开自己的进程令牌、读 `TokenElevation`，随即关掉令牌句柄。
//! 不做任何提权动作——"以管理员身份重启"是另一件事（`ShellExecuteW` 的 `runas`）。

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 当前进程是否提升过（管理员）。
///
/// 读不到令牌时按"未提权"处理：界面据此提示"以管理员身份重启"，
/// 比反过来谎报已提权安全。
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 以管理员身份重新启动当前程序（C# `OnRestartElevated`）。
///
/// 成功时返回子进程 pid 的字符串形式由调用方决定是否退出；这里只负责发起。
/// `runas` 会弹 UAC；被用户取消时返回错误（`ERROR_CANCELLED`）。
pub fn restart_elevated(extra_args: &[String]) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let args = HSTRING::from(extra_args.join(" "));

    let result = unsafe {
        ShellExecuteW(
            None,
            &verb,
            &file,
            &args,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 约定：返回值 > 32 表示成功。
    if result.0 as isize > 32 {
        Ok(())
    } else {
        // 返回值 <= 32 本身就是错误码（UAC 被取消时是 5 / ERROR_ACCESS_DENIED）。
        Err(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(result.0 as u32),
        ))
    }
}
