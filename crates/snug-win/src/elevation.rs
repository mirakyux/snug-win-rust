//! 当前进程是否以管理员身份运行（C# 版在设置窗里用 `WindowsIdentity`/清单判断）。
//!
//! 只读查询：打开自己的进程令牌、读 `TokenElevation`，随即关掉令牌句柄。
//! 不做任何提权动作——"以管理员身份重启"是另一件事（`ShellExecuteW` 的 `runas`）。

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// 当前进程是否提升过（管理员）。
///
/// 读不到令牌时按"未提权"处理：界面据此提示"以管理员身份重启"，
/// 比反过来谎报已提权安全。
pub fn is_elevated() -> bool {
    try_is_elevated().unwrap_or(false)
}

/// Security-sensitive callers must reject a failed token query instead of assuming medium integrity.
pub fn try_is_elevated() -> std::io::Result<bool> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(std::io::Error::other)?;
        let _guard = OwnedHandle::from_raw_handle(token.0);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .map_err(std::io::Error::other)?;
        Ok(elevation.TokenIsElevated != 0)
    }
}

/// 以管理员身份重新启动当前程序（C# `OnRestartElevated`）。
///
/// 成功仅表示 Shell 已接受启动请求，由调用方决定是否退出。
/// `runas` 会弹 UAC；被用户取消时返回错误（`ERROR_CANCELLED`）。
pub fn restart_elevated(extra_args: &[String]) -> windows::core::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::HSTRING;

    let exe = std::env::current_exe()
        .map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let parameters = windows_parameters(extra_args).map_err(|error| {
        windows::core::Error::new(windows::Win32::Foundation::E_INVALIDARG, error.to_string())
    })?;
    let args = HSTRING::from(parameters);

    let result = unsafe { ShellExecuteW(None, &verb, &file, &args, None, SW_SHOWNORMAL) };
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

/// Run a fixed operation in this executable as an elevated, short-lived process.
/// The caller waits off the GUI thread; no shell, executable path or script is supplied by IPC.
pub fn launch_elevated_current_exe(
    args: &[String],
) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR};

    let executable = std::env::current_exe()?;
    let file = HSTRING::from(executable.as_os_str());
    let verb = HSTRING::from("runas");
    let parameters = HSTRING::from(windows_parameters(args)?);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(std::io::Error::other)?;
    if info.hProcess.is_invalid() {
        return Err(std::io::Error::other("管理员任务没有返回进程句柄"));
    }
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(info.hProcess.0) })
}

pub fn run_elevated_current_exe(args: &[String]) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let process = launch_elevated_current_exe(args)?;
    let handle = HANDLE(process.as_raw_handle());
    if unsafe { WaitForSingleObject(handle, 15 * 60 * 1000) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "管理员任务未在 15 分钟内完成",
        ));
    }
    let mut code = 0;
    unsafe { GetExitCodeProcess(handle, &mut code) }.map_err(std::io::Error::other)?;
    if code != 0 {
        return Err(std::io::Error::other(format!(
            "管理员任务失败（退出码 {code}），请查看任务错误提示"
        )));
    }
    Ok(())
}

struct ProcessHandle(windows::Win32::Foundation::HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Authenticate another instance before communicating with the privileged VDD broker.
pub fn is_same_executable_process(pid: u32) -> std::io::Result<bool> {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let process = ProcessHandle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(|error| {
            std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32)
        })?,
    );
    same_executable_handle(process.0)
}

fn same_executable_handle(process: windows::Win32::Foundation::HANDLE) -> std::io::Result<bool> {
    use windows::Win32::System::Threading::{PROCESS_NAME_FORMAT, QueryFullProcessImageNameW};
    use windows::core::PWSTR;

    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(std::io::Error::other)?;
    let actual = std::fs::canonicalize(String::from_utf16_lossy(&path[..length as usize]))?;
    let expected = std::fs::canonicalize(std::env::current_exe()?)?;
    Ok(actual
        .as_os_str()
        .eq_ignore_ascii_case(expected.as_os_str()))
}

/// Wait for a previous GUI instance to release its lock after an explicit elevated restart.
pub fn wait_for_same_executable_process(pid: u32) -> std::io::Result<()> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    if pid == 0 || pid == std::process::id() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "只能等待同一程序的先前实例",
        ));
    }
    // Hold one handle through identity validation and waiting so a recycled PID cannot
    // change the process being waited on after validation.
    let process = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    } {
        Ok(handle) => ProcessHandle(handle),
        // The previous instance may finish shutting down while UAC is being displayed.
        Err(error) if (error.code().0 as u32 & 0xffff) == 87 => return Ok(()),
        Err(error) => {
            return Err(std::io::Error::from_raw_os_error(
                (error.code().0 as u32 & 0xffff) as i32,
            ));
        }
    };
    if !same_executable_handle(process.0)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "先前实例不是同一个程序",
        ));
    }
    if unsafe { WaitForSingleObject(process.0, 30000) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "先前实例尚未退出",
        ));
    }
    Ok(())
}

/// Quote data arguments for a native Windows command line; no command shell is involved.
fn windows_parameters(args: &[String]) -> std::io::Result<String> {
    let mut parameters = Vec::with_capacity(args.len());
    for arg in args {
        if arg.contains('\0') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "argument contains NUL",
            ));
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0;
        for ch in arg.chars() {
            if ch == '\\' {
                backslashes += 1;
                continue;
            }
            if ch == '"' {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
            }
            quoted.push(ch);
            backslashes = 0;
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
        quoted.push('"');
        parameters.push(quoted);
    }
    Ok(parameters.join(" "))
}

/// Display fatal startup failures even when the main window is hidden or never created.
pub fn show_error_dialog(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;

    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from("Snug"),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::{is_same_executable_process, wait_for_same_executable_process, windows_parameters};

    #[test]
    fn parent_validation_rejects_other_or_missing_processes_and_self_waits() {
        assert_eq!(super::try_is_elevated().unwrap(), super::is_elevated());
        assert!(is_same_executable_process(std::process::id()).unwrap());
        assert!(is_same_executable_process(0).is_err());
        assert!(wait_for_same_executable_process(std::process::id()).is_err());
        assert!(wait_for_same_executable_process(0).is_err());
    }

    #[test]
    fn windows_arguments_round_trip_through_the_platform_parser() {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        use windows::core::PCWSTR;

        let args = [
            "",
            "--minimized",
            "a b",
            "a\tb",
            "引号\"路径",
            "C:\\with space\\",
            "a\\\"b",
            "&|;$(echo)",
        ]
        .map(str::to_string);
        let command_line: Vec<u16> = format!("snug.exe {}", windows_parameters(&args).unwrap())
            .encode_utf16()
            .chain([0])
            .collect();
        let mut count = 0;
        let argv = unsafe { CommandLineToArgvW(PCWSTR(command_line.as_ptr()), &mut count) };
        assert!(!argv.is_null());
        let parsed: Vec<String> = unsafe { std::slice::from_raw_parts(argv, count as usize) }
            .iter()
            .skip(1)
            .map(|arg| unsafe { arg.to_string().unwrap() })
            .collect();
        unsafe { LocalFree(Some(HLOCAL(argv.cast()))) };
        assert_eq!(parsed, args);
        assert!(windows_parameters(&["embedded\0nul".into()]).is_err());
        assert_eq!(windows_parameters(&[]).unwrap(), "");
    }
}
