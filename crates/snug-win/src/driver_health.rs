use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_PropertyW, CM_Get_DevNode_Status,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_PROB, CR_SUCCESS, DN_STARTED,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverVersion, DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows::core::PCWSTR;

use crate::vdd::{self, VddError, VddProtocol};
use crate::vdd_broker::{self, BrokerStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeAccess {
    Missing,
    Direct(VddProtocol),
    RequiresElevation,
    ElevatedAccessDenied,
    Busy,
    Unavailable,
}

impl PipeAccess {
    fn for_process(self, elevated: Option<bool>) -> Self {
        match (self, elevated) {
            (Self::RequiresElevation, Some(true)) => Self::ElevatedAccessDenied,
            (Self::RequiresElevation, None) => Self::Unavailable,
            (access, _) => access,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DriverHealth {
    pub installed: bool,
    pub version: Option<String>,
    pub started: Option<bool>,
    pub problem_code: Option<u32>,
    pub access: PipeAccess,
}

impl DriverHealth {
    /// Queries PnP metadata and opens then closes the pipe without sending commands.
    pub fn probe() -> Self {
        let instance_id = crate::display::virtual_display_instance_id();
        let mut health = Self {
            installed: instance_id.is_some(),
            version: None,
            started: None,
            problem_code: None,
            access: vdd::probe_access(vdd::DEFAULT_PIPE_NAME)
                .for_process(crate::elevation::try_is_elevated().ok()),
        };
        let Some(instance_id) = instance_id else {
            return health;
        };
        let wide = crate::wide::to_wide(&instance_id);
        let mut node = 0;
        if unsafe { CM_Locate_DevNodeW(&mut node, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) }
            != CR_SUCCESS
        {
            return health;
        }
        let mut status = CM_DEVNODE_STATUS_FLAGS::default();
        let mut problem = CM_PROB::default();
        if unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, node, 0) } == CR_SUCCESS {
            health.started = Some(status.0 & DN_STARTED.0 != 0);
            health.problem_code = (problem.0 != 0).then_some(problem.0);
        }
        let mut value = [0u16; 64];
        let mut bytes = std::mem::size_of_val(&value) as u32;
        let mut kind = DEVPROPTYPE::default();
        if unsafe {
            CM_Get_DevNode_PropertyW(
                node,
                &DEVPKEY_Device_DriverVersion,
                &mut kind,
                Some(value.as_mut_ptr().cast()),
                &mut bytes,
                0,
            )
        } == CR_SUCCESS
            && kind == DEVPROP_TYPE_STRING
            && bytes > 2
            && bytes as usize <= std::mem::size_of_val(&value)
        {
            health.version = Some(crate::wide::from_wide(&value[..bytes as usize / 2]));
        }
        health
    }
}

/// Connect directly or request driver authorization during interactive use.
pub fn retry_connection() -> Result<VddProtocol, String> {
    retry_connection_with_broker(vdd_broker::retry_connection)
}

/// Connect from login startup using direct access or the account's previously authorized broker.
pub fn retry_connection_without_prompt() -> Result<VddProtocol, String> {
    retry_connection_with_broker(vdd_broker::retry_connection_without_prompt)
}

fn retry_connection_with_broker(
    connect_broker: impl FnOnce() -> std::io::Result<Option<VddProtocol>>,
) -> Result<VddProtocol, String> {
    let elevated = crate::elevation::try_is_elevated()
        .map_err(|error| format!("无法确认当前进程权限：{error}"))?;
    match vdd::probe_access(vdd::DEFAULT_PIPE_NAME).for_process(Some(elevated)) {
        PipeAccess::Direct(_) => vdd::VddClient::direct(vdd::DEFAULT_PIPE_NAME)
            .probe_connection()
            .map_err(|error| describe_direct_error(elevated, error)),
        PipeAccess::ElevatedAccessDenied => Err(ELEVATED_ACCESS_DENIED.into()),
        PipeAccess::RequiresElevation => connect_broker()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "驱动未返回协议类型".into()),
        PipeAccess::Missing => Err("驱动控制管道尚未出现".into()),
        PipeAccess::Busy => Err("驱动控制管道忙，请稍后再试".into()),
        PipeAccess::Unavailable => Err("当前账户无法打开驱动控制管道".into()),
    }
}

const ELEVATED_ACCESS_DENIED: &str =
    "当前进程已是管理员，驱动控制管道仍拒绝访问。请检查驱动权限或尝试修复驱动。";

fn describe_direct_error(elevated: bool, error: VddError) -> String {
    let permission_denied = match &error {
        VddError::Connect { source, .. } | VddError::Io(source) => {
            source.kind() == std::io::ErrorKind::PermissionDenied
        }
        VddError::NotConnected | VddError::OutcomeUnknown { .. } => false,
    };
    if elevated && permission_denied {
        ELEVATED_ACCESS_DENIED.into()
    } else {
        error.to_string()
    }
}

pub fn broker_status() -> BrokerStatus {
    vdd_broker::status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_denial_only_requests_authorization_for_a_standard_process() {
        assert_eq!(
            PipeAccess::RequiresElevation.for_process(Some(true)),
            PipeAccess::ElevatedAccessDenied
        );
        assert_eq!(
            PipeAccess::RequiresElevation.for_process(Some(false)),
            PipeAccess::RequiresElevation
        );
        assert_eq!(
            PipeAccess::RequiresElevation.for_process(None),
            PipeAccess::Unavailable
        );
        let direct = PipeAccess::Direct(VddProtocol::Legacy);
        assert_eq!(direct.clone().for_process(Some(true)), direct);
    }

    #[test]
    fn elevated_failures_between_probe_and_connection_explain_the_pipe_denial() {
        let denied = || VddError::Connect {
            name: vdd::DEFAULT_PIPE_NAME.into(),
            source: std::io::Error::from_raw_os_error(5),
        };
        assert_eq!(
            describe_direct_error(true, denied()),
            ELEVATED_ACCESS_DENIED
        );
        assert_ne!(
            describe_direct_error(false, denied()),
            ELEVATED_ACCESS_DENIED
        );
    }
}
