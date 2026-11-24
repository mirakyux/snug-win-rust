//! Read-only baseline for the standard-user GUI. Never add/remove screens or change modes.
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VALIDATE,
    SetDisplayConfig,
};

fn validate_current_display_configuration() -> Result<u32, u32> {
    let (mut paths_len, mut modes_len) = (0, 0);
    let code = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_len, &mut modes_len)
    }
    .0;
    if code != 0 {
        return Err(code);
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_len as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_len as usize];
    let code = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_len,
            paths.as_mut_ptr(),
            &mut modes_len,
            modes.as_mut_ptr(),
            None,
        )
    }
    .0;
    if code != 0 {
        return Err(code);
    }
    paths.truncate(paths_len as usize);
    modes.truncate(modes_len as usize);
    // SDC_VALIDATE explicitly validates without applying; no SDC_APPLY or database-save flags.
    Ok(unsafe {
        SetDisplayConfig(
            Some(&paths),
            Some(&modes),
            SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VALIDATE,
        )
    } as u32)
}

fn main() -> std::io::Result<()> {
    if snug_win::is_elevated() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Run this read-only capability probe from a standard-user terminal",
        ));
    }
    let path = format!(r"\\.\pipe\{}", snug_win::vdd::DEFAULT_PIPE_NAME);
    let bidirectional = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path);
    let (driver_access, driver_error) = match bidirectional {
        Ok(pipe) => {
            drop(pipe);
            ("read-write", None)
        }
        Err(_) => match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(pipe) => {
                drop(pipe);
                ("write-only", None)
            }
            Err(error) => ("unavailable", error.raw_os_error()),
        },
    };
    let display_validation = match validate_current_display_configuration() {
        Ok(code) => serde_json::json!({ "query": "ok", "validate_status": code }),
        Err(code) => serde_json::json!({ "query": "failed", "query_status": code }),
    };
    let health = snug_win::driver_health::DriverHealth::probe();
    println!(
        "{}",
        serde_json::json!({
            "elevated": false,
            "driver_access": driver_access,
            "driver_error": driver_error,
            "driver_health": {
                "installed": health.installed,
                "version": health.version,
                "started": health.started,
                "problem_code": health.problem_code,
                "access": format!("{:?}", health.access),
            },
            "display_validation": display_validation,
            "topology_mutations": 0,
            "driver_commands_sent": 0,
            "elevated_window_control": "requires explicit administrator compatibility mode"
        })
    );
    Ok(())
}
