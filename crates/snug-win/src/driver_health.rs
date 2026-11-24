use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_PropertyW, CM_Get_DevNode_Status,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CM_PROB, CR_SUCCESS, DN_STARTED,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverVersion, DEVPROP_TYPE_STRING, DEVPROPTYPE,
};
use windows::core::PCWSTR;

use crate::vdd::{self, VddProtocol};
use crate::vdd_broker::{self, BrokerStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeAccess {
    Missing,
    Direct(VddProtocol),
    RequiresElevation,
    Busy,
    Unavailable,
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
            access: vdd::probe_access(vdd::DEFAULT_PIPE_NAME),
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

/// GUI calls this on a background thread after an explicit click.
pub fn retry_connection() -> Result<VddProtocol, String> {
    match vdd::probe_access(vdd::DEFAULT_PIPE_NAME) {
        PipeAccess::Direct(_) => vdd::VddClient::direct(vdd::DEFAULT_PIPE_NAME)
            .probe_connection()
            .map_err(|error| error.to_string()),
        PipeAccess::RequiresElevation if !crate::is_elevated() => vdd_broker::retry_connection()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "驱动未返回协议类型".into()),
        PipeAccess::Missing => Err("驱动控制管道尚未出现".into()),
        PipeAccess::Busy => Err("驱动控制管道忙，请稍后再试".into()),
        _ => Err("当前账户无法打开驱动控制管道".into()),
    }
}

pub fn broker_status() -> BrokerStatus {
    vdd_broker::status()
}
