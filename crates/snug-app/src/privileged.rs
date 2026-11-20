//! One-shot elevated operations. Run before loading user configuration or creating the GUI.
//! No command, executable path, staging path, script or download URL is accepted as input.

use std::io;

const INSTALL_DRIVER: &str = "--snug-helper-install-driver";
const VDD_BROKER: &str = "--snug-helper-vdd";

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    InstallDriver,
    VddBroker { parent: u32, pipe: String },
}

fn parse(args: &[String]) -> io::Result<Option<Operation>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "管理员任务参数无效");
    match args.first().map(String::as_str) {
        Some(INSTALL_DRIVER) if args.len() == 1 => Ok(Some(Operation::InstallDriver)),
        Some(VDD_BROKER) if args.len() == 3 => {
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
            }))
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
        Operation::VddBroker { parent, pipe } => snug_win::vdd_broker::serve(parent, &pipe)?,
        Operation::InstallDriver => {
            if !crate::driver::install_privileged() {
                return Err(io::Error::other("驱动安装未成功，请重试或检查设备状态"));
            }
        }
    }
    Ok(true)
}

pub fn install_driver() -> io::Result<()> {
    snug_win::elevation::run_elevated_current_exe(&[INSTALL_DRIVER.into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_operations_accept_only_fixed_typed_requests() {
        assert_eq!(
            parse(&[INSTALL_DRIVER.into()]).unwrap(),
            Some(Operation::InstallDriver)
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
                pipe: pipe.into()
            })
        );
        for args in [
            vec![INSTALL_DRIVER, "C:\\any-script.ps1"],
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
            vec!["--minimized", INSTALL_DRIVER],
            vec!["--snug-helper-arbitrary-command"],
        ] {
            assert!(parse(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
        assert!(parse(&["--minimized".into()]).unwrap().is_none());
    }
}
