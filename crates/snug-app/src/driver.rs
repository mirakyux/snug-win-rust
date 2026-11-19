//! 虚拟屏驱动的自动安装。
//!
//! 用户不该为了装个驱动先去别处找文件、也不该知道 `\\\\.\\pipe\\virtualdisplaydriver`
//! 这种东西。程序本身清单就是 `requireAdministrator`，所以**能自己装**：
//!
//! 1. 启动时探一下命名管道（`snug_win::vdd::is_driver_pipe_available`）；
//! 2. 没有，就在后台跑打包进来的 `driver\Install-SnugDriver.ps1 -Action Install`；
//! 3. 脚本自己校验安装包与证书的 SHA-256，装完等管道起来，然后把手让给建屏流程。
//!
//! 全程在后台线程：装驱动要几十秒（msiexec），不能卡住界面。
//! 失败了也不拦着程序启动——建屏的重试阶梯还在，设置页里也能手动再来一次。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 装完等管道就绪的上限。msiexec 装完到服务起管道通常几秒，给足余量。
const PIPE_WAIT: Duration = Duration::from_secs(40);

/// 从程序目录往上找 `driver\Install-SnugDriver.ps1`（C# `LocateBundledDriverScript`）。
///
/// 发布包把它放在 exe 同级的 `driver\`；开发时从 `target\release\` 出发也能找到仓库根的。
pub fn locate_script() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent().map(Path::to_path_buf)?;
    for _ in 0..5 {
        let candidate = dir.join("driver").join("Install-SnugDriver.ps1");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// 驱动管道在不在（也就是"驱动装好并且服务起着"）。
pub fn pipe_available() -> bool {
    snug_win::vdd::is_driver_pipe_available(snug_win::vdd::DEFAULT_PIPE_NAME)
}

/// 没驱动就自动装一次。返回 `true` 表示最后管道可用（本来就可用，或装成功了）。
///
/// 这个函数会阻塞到安装结束，调用方要放在后台线程里。
pub fn ensure_installed() -> bool {
    if pipe_available() {
        return true;
    }
    let Some(script) = locate_script() else {
        crate::warn!("驱动：没有命名管道，也没找到 driver\\Install-SnugDriver.ps1（程序目录及其上 5 层）");
        return false;
    };
    crate::info!("驱动：检测到虚拟显示器驱动未就绪，自动安装（{}）", script.display());
    let started = Instant::now();
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
            "-Action",
            "Install",
        ])
        .creation_flags_hidden()
        .output();
    match output {
        Ok(output) => {
            let ms = started.elapsed().as_millis();
            let text = String::from_utf8_lossy(&output.stdout);
            let err = String::from_utf8_lossy(&output.stderr);
            for line in text.lines().chain(err.lines()).filter(|line| !line.trim().is_empty()) {
                crate::info!("驱动：{line}");
            }
            if !output.status.success() {
                crate::warn!(
                    "驱动：自动安装失败（退出码 {}，{ms} ms）",
                    output.status.code().unwrap_or(-1)
                );
                return pipe_available();
            }
            crate::info!("驱动：安装脚本结束（{ms} ms），等命名管道就绪…");
        }
        Err(error) => {
            crate::warn!("驱动：拉起安装脚本失败：{error}");
            return false;
        }
    }

    // 脚本自己会等 15 秒；再兜一层，覆盖"装完了但服务还没起管道"的情况。
    let deadline = Instant::now() + PIPE_WAIT;
    while Instant::now() < deadline {
        if pipe_available() {
            crate::info!("驱动：命名管道已就绪，可以建手机屏了");
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    crate::warn!("驱动：装完了但命名管道还没起来（可能重启 Windows 后才生效）");
    false
}

/// 起一个后台线程做 [`ensure_installed`]，不挡启动。
pub fn spawn_ensure_installed() {
    std::thread::Builder::new()
        .name("snug-driver-install".to_string())
        .spawn(|| {
            let _ = ensure_installed();
        })
        .ok();
}

/// `CREATE_NO_WINDOW`：别给用户闪一个蓝框 PowerShell 出来。
trait HiddenWindow {
    fn creation_flags_hidden(&mut self) -> &mut Self;
}

impl HiddenWindow for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_script_walks_up_to_five_levels() {
        // 只在仓库里跑得通：测试二进制在 target\debug\deps 下，往上能找到仓库根的 driver\。
        let found = locate_script();
        if let Some(path) = found {
            assert!(path.ends_with("Install-SnugDriver.ps1"));
            assert!(path.is_file());
        }
    }

    #[test]
    fn pipe_probe_does_not_panic() {
        // 只是探一下，真机有没有驱动都行：不能崩。
        let _ = pipe_available();
    }
}
