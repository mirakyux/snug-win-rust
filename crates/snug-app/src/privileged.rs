//! One-shot elevated operations. Run before loading user configuration or creating the GUI.
//! No command, executable path, staging path, script or download URL is accepted as input.

use std::io;

const INSTALL_DRIVER: &str = "--snug-helper-install-driver";
const INSTALL_DRIVER_FOR_LOGIN: &str = "--snug-helper-install-driver-for-login";
const VDD_BROKER: &str = "--snug-helper-vdd";
const SCHEDULED_BROKER: &str = "--snug-helper-vdd-task";
const ENABLE_UNATTENDED: &str = "--snug-helper-enable-unattended";
const DISABLE_UNATTENDED: &str = "--snug-helper-disable-unattended";

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    InstallDriver { login_parent: Option<u32> },
    VddBroker { parent: u32, pipe: String, scheduled: bool },
    SetUnattended { parent: u32, enabled: bool },
}

fn parse(args: &[String]) -> io::Result<Option<Operation>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "管理员任务参数无效");
    match args.first().map(String::as_str) {
        Some(INSTALL_DRIVER) if args.len() == 1 => Ok(Some(Operation::InstallDriver { login_parent: None })),
        Some(INSTALL_DRIVER_FOR_LOGIN) if args.len() == 2 => {
            let parent = args[1].parse::<std::num::NonZeroU32>().map_err(|_| invalid())?;
            Ok(Some(Operation::InstallDriver { login_parent: Some(parent.get()) }))
        }
        Some(flag @ (VDD_BROKER | SCHEDULED_BROKER)) if args.len() == 3 => {
            let parent = args[1].parse::<u32>().map_err(|_| invalid())?;
            let prefix = format!("snug-vdd-{parent}-");
            let nonce = args[2].strip_prefix(&prefix).ok_or_else(invalid)?;
            if parent == 0
                || nonce.len() != 32
                || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid());
            }
            Ok(Some(Operation::VddBroker {
                parent,
                pipe: args[2].clone(),
                scheduled: flag == SCHEDULED_BROKER,
            }))
        }
        Some(flag @ (ENABLE_UNATTENDED | DISABLE_UNATTENDED)) if args.len() == 2 => {
            let parent = args[1].parse::<std::num::NonZeroU32>().map_err(|_| invalid())?;
            Ok(Some(Operation::SetUnattended { parent: parent.get(), enabled: flag == ENABLE_UNATTENDED }))
        }
        Some(flag) if flag.starts_with("--snug-helper") => Err(invalid()),
        _ if args.iter().any(|arg| arg.starts_with("--snug-helper")) => Err(invalid()),
        _ => Ok(None),
    }
}

/// The GUI observes broker exit through its authenticated process handle and logs failures.
/// A background helper must never stay alive in a modal error dialog after its GUI exits.
pub fn is_background_request(args: &[String]) -> bool {
    matches!(parse(args), Ok(Some(Operation::VddBroker { .. })))
}

pub fn dispatch(args: &[String]) -> io::Result<bool> {
    let Some(operation) = parse(args)? else {
        return Ok(false);
    };
    if !snug_win::is_elevated() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "此一次性任务必须通过 UAC 提升权限",
        ));
    }
    match operation {
        Operation::VddBroker { parent, pipe, scheduled: true } => snug_win::vdd_broker::serve_scheduled(parent, &pipe)?,
        Operation::VddBroker { parent, pipe, scheduled: false } => snug_win::vdd_broker::serve(parent, &pipe)?,
        Operation::SetUnattended { parent, enabled } => snug_win::vdd_task::set_enabled(enabled, parent)?,
        Operation::InstallDriver { login_parent } => {
            if !crate::driver::install_privileged() {
                return Err(io::Error::other("驱动安装未成功，请重试或检查设备状态"));
            }
            if let Some(parent) = login_parent {
                snug_win::vdd_task::set_enabled(true, parent)?;
            }
        }
    }
    Ok(true)
}

pub fn install_driver() -> io::Result<()> {
    snug_win::elevation::run_elevated_current_exe(&[INSTALL_DRIVER.into()])
}

pub fn install_driver_for_login() -> io::Result<()> {
    run_login_operation(INSTALL_DRIVER_FOR_LOGIN)
}

pub fn set_unattended_driver(enabled: bool) -> io::Result<()> {
    if snug_win::elevation::try_is_elevated()? {
        return snug_win::vdd_task::set_enabled_from_elevated_app(enabled);
    }
    run_login_operation(if enabled { ENABLE_UNATTENDED } else { DISABLE_UNATTENDED })
}

fn run_login_operation(operation: &str) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    // Keep the enrollment image pinned during elevation and until the protected copy is registered.
    let _image = std::fs::OpenOptions::new().read(true).share_mode(1).open(std::env::current_exe()?)?;
    snug_win::elevation::run_elevated_current_exe(&[operation.into(), std::process::id().to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_operations_accept_only_fixed_typed_requests() {
        assert_eq!(
            parse(&[INSTALL_DRIVER.into()]).unwrap(),
            Some(Operation::InstallDriver { login_parent: None })
        );
        assert_eq!(
            parse(&[INSTALL_DRIVER_FOR_LOGIN.into(), "42".into()]).unwrap(),
            Some(Operation::InstallDriver { login_parent: Some(42) })
        );
        let pipe = "snug-vdd-42-0123456789abcdef0123456789abcdef";
        assert!(is_background_request(&[
            VDD_BROKER.into(),
            "42".into(),
            pipe.into()
        ]));
        assert!(!is_background_request(&[INSTALL_DRIVER.into()]));
        assert!(!is_background_request(&[VDD_BROKER.into()]));
        assert_eq!(
            parse(&[VDD_BROKER.into(), "42".into(), pipe.into()]).unwrap(),
            Some(Operation::VddBroker {
                parent: 42,
                pipe: pipe.into(),
                scheduled: false,
            })
        );
        for args in [
            vec![INSTALL_DRIVER, "C:\\any-script.ps1"],
            vec![INSTALL_DRIVER_FOR_LOGIN, "0"],
            vec![INSTALL_DRIVER_FOR_LOGIN, "42", "C:\\any-script.ps1"],
            vec![
                "--snug-helper-update",
                "42",
                "https://example.invalid/payload",
                "normal",
            ],
            vec!["--snug-helper-update", "42", "1.3.1", "normal"],
            vec![
                VDD_BROKER,
                "0",
                "snug-vdd-0-0123456789abcdef0123456789abcdef",
            ],
            vec![VDD_BROKER, "43", pipe],
            vec![
                VDD_BROKER,
                "42",
                "snug-vdd-42-0123456789abcdef0123456789abcde/",
            ],
            vec![VDD_BROKER, "42", pipe, "unexpected"],
            vec![SCHEDULED_BROKER, "42", pipe, INSTALL_DRIVER],
            vec![ENABLE_UNATTENDED, "0"],
            vec![DISABLE_UNATTENDED, "42", "unexpected"],
            vec!["--minimized", INSTALL_DRIVER],
            vec!["--snug-helper-arbitrary-command"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
        assert!(parse(&["--minimized".into()]).unwrap().is_none());
        assert_eq!(parse(&[ENABLE_UNATTENDED.into(), "42".into()]).unwrap(), Some(Operation::SetUnattended { parent: 42, enabled: true }));
    }
}
