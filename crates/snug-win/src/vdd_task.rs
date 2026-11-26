//! One-time elevated enrollment for the on-demand virtual display broker.

use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::{fs, io};

use windows::Win32::Foundation::{
    HANDLE, HLOCAL, LocalFree, RPC_E_CHANGED_MODE, VARIANT_FALSE, VARIANT_TRUE,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
    SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, GetFinalPathNameByHandleW,
    READ_CONTROL, VOLUME_NAME_NT, WRITE_DAC,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    SAFEARRAY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement};
use windows::Win32::System::ProcessStatus::K32GetMappedFileNameW;
use windows::Win32::System::TaskScheduler::*;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::System::Variant::{VARIANT, VT_ARRAY, VT_BSTR};
use windows::core::{BSTR, HSTRING, Interface};

use crate::vdd_broker::ProcessIdentity;

const TASK_PREFIX: &str = "Snug-VddBroker-";
const HELPER_NAME: &str = "snug-vdd-broker.exe";
const HELPER_ARGUMENTS: &str = "--snug-helper-vdd-task $(Arg0) $(Arg1)";
const DIRECTORY_ACL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;RC;;;OW)";
const HELPER_ACL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;RC;;;OW)";
const TASK_ACL_BASE: &str = "O:BAG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;RC;;;OW)";

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error)
}

struct Com(bool);

impl Com {
    fn enter() -> io::Result<Self> {
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if status == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        status.ok().map_err(windows_error)?;
        Ok(Self(true))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn connect() -> io::Result<(ITaskService, ITaskFolder)> {
    unsafe {
        let service: ITaskService =
            CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?;
        let empty = VARIANT::default();
        service
            .Connect(&empty, &empty, &empty, &empty)
            .map_err(windows_error)?;
        let folder = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(windows_error)?;
        Ok((service, folder))
    }
}

fn current_identity() -> io::Result<ProcessIdentity> {
    ProcessIdentity::open(std::process::id())
}

fn task_name(sid: &str) -> String {
    format!("{TASK_PREFIX}{sid}")
}

fn helper_path(sid: &str) -> io::Result<PathBuf> {
    Ok(crate::system_paths::system_directory()?
        .join(task_name(sid))
        .join(HELPER_NAME))
}

pub(crate) fn executable() -> io::Result<PathBuf> {
    helper_path(current_identity()?.sid())
}

fn task_acl(sid: &str) -> String {
    // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE. The protected ACL also removes implicit owner writes.
    format!("{TASK_ACL_BASE}(A;;0x1200a9;;;{sid})")
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn parse(sddl: &str) -> io::Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                1,
                &mut descriptor,
                None,
            )
        }
        .map_err(windows_error)?;
        Ok(Self(descriptor))
    }

    fn protected_aces(&self) -> io::Result<Vec<Vec<u8>>> {
        let dacl = self.protected_dacl()?;
        let mut aces = Vec::new();
        for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
            let mut ace = null_mut();
            unsafe { GetAce(dacl, index, &mut ace) }.map_err(windows_error)?;
            let header = unsafe { &*ace.cast::<windows::Win32::Security::ACE_HEADER>() };
            aces.push(
                unsafe {
                    std::slice::from_raw_parts(ace.cast::<u8>(), usize::from(header.AceSize))
                }
                .to_vec(),
            );
        }
        aces.sort_unstable();
        Ok(aces)
    }

    fn protected_dacl(&self) -> io::Result<*mut ACL> {
        let mut control = 0;
        let mut revision = 0;
        unsafe { GetSecurityDescriptorControl(self.0, &mut control, &mut revision) }
            .map_err(windows_error)?;
        if control & SE_DACL_PROTECTED.0 == 0 {
            return Err(denied("驱动权限代理的访问列表未受保护"));
        }
        let mut present = Default::default();
        let mut defaulted = Default::default();
        let mut dacl: *mut ACL = null_mut();
        unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
            .map_err(windows_error)?;
        if !present.as_bool() || dacl.is_null() {
            return Err(denied("驱动权限代理的访问列表缺失"));
        }
        Ok(dacl)
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

fn require_acl(actual: &SecurityDescriptor, expected_sddl: &str) -> io::Result<()> {
    if actual.protected_aces()? != SecurityDescriptor::parse(expected_sddl)?.protected_aces()? {
        return Err(denied("驱动权限代理的访问列表与预期不符"));
    }
    Ok(())
}

fn file_descriptor(file: &fs::File) -> io::Result<SecurityDescriptor> {
    let mut actual = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut actual),
        )
    };
    if status.0 != 0 {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(SecurityDescriptor(actual))
}

fn open_protected_directory(path: &Path) -> io::Result<fs::File> {
    let attrs = fs::symlink_metadata(path)?;
    if !attrs.is_dir() || attrs.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(denied("驱动权限代理目录不是普通目录"));
    }
    let directory = fs::OpenOptions::new()
        .access_mode((READ_CONTROL | FILE_READ_ATTRIBUTES).0)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    if directory.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(denied("驱动权限代理目录包含重解析点"));
    }
    require_acl(&file_descriptor(&directory)?, DIRECTORY_ACL)?;
    Ok(directory)
}

fn mapped_executable() -> io::Result<fs::File> {
    // Hold a read handle that denies writes and replacement while comparing it to the loaded image.
    let source = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(std::env::current_exe()?)?;
    let mut mapped = vec![0_u16; 32768];
    let mut opened = vec![0_u16; 32768];
    let module = unsafe { GetModuleHandleW(None) }.map_err(windows_error)?;
    let mapped_len =
        unsafe { K32GetMappedFileNameW(GetCurrentProcess(), module.0, &mut mapped) } as usize;
    let opened_len = unsafe {
        GetFinalPathNameByHandleW(HANDLE(source.as_raw_handle()), &mut opened, VOLUME_NAME_NT)
    } as usize;
    if mapped_len == 0
        || opened_len == 0
        || mapped_len >= mapped.len()
        || opened_len >= opened.len()
        || !String::from_utf16_lossy(&mapped[..mapped_len])
            .eq_ignore_ascii_case(&String::from_utf16_lossy(&opened[..opened_len]))
    {
        return Err(denied("无法确认复制来源与当前运行的 Snug 一致"));
    }
    Ok(source)
}

fn existing_regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(info)
            if info.is_file() && info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 =>
        {
            Ok(true)
        }
        Ok(_) => Err(denied("驱动权限代理路径不是普通文件")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn install_helper(path: &Path) -> io::Result<()> {
    let mut source = mapped_executable()?;
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("驱动权限代理路径无效"))?;
    match crate::system_paths::create_protected_directory(directory) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error),
    }
    let _directory = open_protected_directory(directory)?;
    if existing_regular_file(path)? {
        let mut installed = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(path)?;
        if require_acl(&file_descriptor(&installed)?, HELPER_ACL).is_ok()
            && same_contents(&mut source, &mut installed)?
        {
            return Ok(());
        }
        source.seek(SeekFrom::Start(0))?;
    }
    let staged = directory.join(format!("snug-stage-{}.tmp", std::process::id()));
    let mut destination = fs::OpenOptions::new()
        .access_mode((FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC).0)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(&staged)?;
    let copied = protect_helper(&destination)
        .and_then(|()| io::copy(&mut source, &mut destination))
        .and_then(|_| destination.sync_all());
    drop(destination);
    let result = copied.and_then(|_| fs::rename(&staged, path));
    if result.is_err() {
        let _ = fs::remove_file(staged);
    }
    result
}

fn protect_helper(file: &fs::File) -> io::Result<()> {
    let expected = SecurityDescriptor::parse(HELPER_ACL)?;
    let dacl = expected.protected_dacl()?;
    let result = unsafe {
        SetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    if result.0 != 0 {
        return Err(io::Error::from_raw_os_error(result.0 as i32));
    }
    require_acl(&file_descriptor(file)?, HELPER_ACL)
}

fn same_contents(left: &mut fs::File, right: &mut fs::File) -> io::Result<bool> {
    let size = left.metadata()?.len();
    if size != right.metadata()?.len() {
        return Ok(false);
    }
    let mut a = [0_u8; 32768];
    let mut b = [0_u8; 32768];
    let mut remaining = size;
    while remaining != 0 {
        let count = remaining.min(a.len() as u64) as usize;
        left.read_exact(&mut a[..count])?;
        right.read_exact(&mut b[..count])?;
        if a[..count] != b[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

fn missing(error: &windows::core::Error) -> bool {
    matches!(error.code().0 as u32, 0x8007_0002 | 0x8007_0003)
}

fn scheduled_task(folder: &ITaskFolder, name: &BSTR) -> io::Result<Option<IRegisteredTask>> {
    match unsafe { folder.GetTask(name) } {
        Ok(task) => Ok(Some(task)),
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(windows_error(error)),
    }
}

fn path_text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("驱动权限代理路径无法表示为 Unicode"))
}

fn verify_task(task: &IRegisteredTask, sid: &str, executable: &Path) -> io::Result<()> {
    unsafe {
        let descriptor = SecurityDescriptor::parse(
            &task
                .GetSecurityDescriptor(
                    (OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION).0 as i32,
                )
                .map_err(windows_error)?
                .to_string(),
        )?;
        require_acl(&descriptor, &task_acl(sid))?;
        let mut owner = PSID::default();
        let mut defaulted = Default::default();
        GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted)
            .map_err(windows_error)?;
        if owner.0.is_null()
            || !IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
                && !IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
        {
            return Err(denied("驱动权限代理任务不属于管理员"));
        }

        let definition = task.Definition().map_err(windows_error)?;
        let principal = definition.Principal().map_err(windows_error)?;
        let mut user = BSTR::new();
        let mut logon = TASK_LOGON_TYPE::default();
        let mut level = TASK_RUNLEVEL_TYPE::default();
        principal.UserId(&mut user).map_err(windows_error)?;
        principal.LogonType(&mut logon).map_err(windows_error)?;
        principal.RunLevel(&mut level).map_err(windows_error)?;
        let mut trigger_count = 0;
        definition
            .Triggers()
            .map_err(windows_error)?
            .Count(&mut trigger_count)
            .map_err(windows_error)?;
        let actions = definition.Actions().map_err(windows_error)?;
        let mut action_count = 0;
        actions.Count(&mut action_count).map_err(windows_error)?;
        if user.to_string() != sid
            || logon != TASK_LOGON_INTERACTIVE_TOKEN
            || level != TASK_RUNLEVEL_HIGHEST
            || trigger_count != 0
            || action_count != 1
        {
            return Err(denied("驱动权限代理任务的账户、触发器或操作不符"));
        }
        let action: IExecAction = actions
            .get_Item(1)
            .map_err(windows_error)?
            .cast()
            .map_err(windows_error)?;
        let mut path = BSTR::new();
        let mut arguments = BSTR::new();
        let mut working = BSTR::new();
        action.Path(&mut path).map_err(windows_error)?;
        action.Arguments(&mut arguments).map_err(windows_error)?;
        action
            .WorkingDirectory(&mut working)
            .map_err(windows_error)?;
        if !path
            .to_string()
            .eq_ignore_ascii_case(path_text(executable)?)
            || arguments.to_string() != HELPER_ARGUMENTS
            || !working
                .to_string()
                .eq_ignore_ascii_case(path_text(executable.parent().unwrap())?)
        {
            return Err(denied("驱动权限代理的启动命令不符"));
        }
    }
    Ok(())
}

fn register(
    service: &ITaskService,
    folder: &ITaskFolder,
    name: &BSTR,
    sid: &str,
    helper: &Path,
) -> io::Result<IRegisteredTask> {
    unsafe {
        let definition = service.NewTask(0).map_err(windows_error)?;
        let principal = definition.Principal().map_err(windows_error)?;
        let user = BSTR::from(sid);
        principal.SetUserId(&user).map_err(windows_error)?;
        principal
            .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
            .map_err(windows_error)?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(windows_error)?;
        let settings = definition.Settings().map_err(windows_error)?;
        settings.SetEnabled(VARIANT_FALSE).map_err(windows_error)?;
        settings
            .SetAllowDemandStart(VARIANT_TRUE)
            .map_err(windows_error)?;
        settings
            .SetDisallowStartIfOnBatteries(VARIANT_FALSE)
            .map_err(windows_error)?;
        settings
            .SetStopIfGoingOnBatteries(VARIANT_FALSE)
            .map_err(windows_error)?;
        settings
            .SetExecutionTimeLimit(&BSTR::from("PT0S"))
            .map_err(windows_error)?;
        settings
            .SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
            .map_err(windows_error)?;
        let action: IExecAction = definition
            .Actions()
            .map_err(windows_error)?
            .Create(TASK_ACTION_EXEC)
            .map_err(windows_error)?
            .cast()
            .map_err(windows_error)?;
        action
            .SetPath(&BSTR::from(path_text(helper)?))
            .map_err(windows_error)?;
        action
            .SetArguments(&BSTR::from(HELPER_ARGUMENTS))
            .map_err(windows_error)?;
        action
            .SetWorkingDirectory(&BSTR::from(path_text(helper.parent().unwrap())?))
            .map_err(windows_error)?;
        let acl = BSTR::from(task_acl(sid));
        folder
            .RegisterTaskDefinition(
                name,
                &definition,
                TASK_CREATE.0 | TASK_DONT_ADD_PRINCIPAL_ACE.0 | TASK_IGNORE_REGISTRATION_TRIGGERS.0,
                &VARIANT::from(user),
                &VARIANT::default(),
                TASK_LOGON_INTERACTIVE_TOKEN,
                &VARIANT::from(acl),
            )
            .map_err(windows_error)
    }
}

/// Register or remove the broker after the current account approves elevation.
pub fn set_enabled(enabled: bool, parent_pid: u32) -> io::Result<()> {
    let caller = current_identity()?;
    let parent = ProcessIdentity::open(parent_pid)?;
    if !caller.elevated()
        || caller.session_id() == 0
        || parent_pid == std::process::id()
        || !parent.is_alive()
        || !caller.same_user_session(&parent)
        || !crate::elevation::is_same_executable_process(parent_pid)?
    {
        return Err(denied("请使用当前登录账户的管理员权限设置驱动授权"));
    }
    set_for_account(enabled, &caller)
}

/// The desktop can enroll directly when it already has an elevated interactive token.
pub fn set_enabled_from_elevated_app(enabled: bool) -> io::Result<()> {
    let caller = current_identity()?;
    if !caller.elevated() || caller.session_id() == 0 {
        return Err(denied("请使用当前登录账户的管理员权限设置驱动授权"));
    }
    set_for_account(enabled, &caller)
}

fn set_for_account(enabled: bool, caller: &ProcessIdentity) -> io::Result<()> {
    let _com = Com::enter()?;
    let (service, folder) = connect()?;
    let sid = caller.sid();
    let name = BSTR::from(task_name(sid));
    let helper = helper_path(sid)?;
    let task = scheduled_task(&folder, &name)?;
    if let Some(task) = &task {
        verify_task(task, sid, &helper)?;
    }
    if enabled {
        install_helper(&helper)?;
        if let Some(task) = task {
            unsafe { task.SetEnabled(VARIANT_TRUE) }.map_err(windows_error)?;
        } else {
            let registered = register(&service, &folder, &name, sid, &helper)?;
            if let Err(error) = verify_task(&registered, sid, &helper).and_then(|()| {
                unsafe { registered.SetEnabled(VARIANT_TRUE) }.map_err(windows_error)
            }) {
                let _ = unsafe { folder.DeleteTask(&name, 0) };
                return Err(error);
            }
        }
    } else {
        if let Some(task) = task {
            unsafe { task.SetEnabled(VARIANT_FALSE) }.map_err(windows_error)?;
            let _ = unsafe { task.Stop(0) };
            unsafe { folder.DeleteTask(&name, 0) }.map_err(windows_error)?;
        }
        let directory = helper.parent().unwrap();
        match open_protected_directory(directory) {
            Ok(directory_handle) => {
                if existing_regular_file(&helper)? {
                    fs::remove_file(&helper)?;
                }
                drop(directory_handle);
                fs::remove_dir(directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Check the scheduler registration visible to this Windows account.
pub fn is_enabled() -> io::Result<bool> {
    let caller = current_identity()?;
    let _com = Com::enter()?;
    let (_, folder) = connect()?;
    let name = BSTR::from(task_name(caller.sid()));
    let Some(task) = scheduled_task(&folder, &name)? else {
        return Ok(false);
    };
    verify_task(&task, caller.sid(), &helper_path(caller.sid())?)?;
    unsafe { task.Enabled() }
        .map(|value| value.as_bool())
        .map_err(windows_error)
}

struct Arguments(*mut SAFEARRAY);

impl Drop for Arguments {
    fn drop(&mut self) {
        let _ = unsafe { SafeArrayDestroy(self.0) };
    }
}

fn valid_arguments(parent: u32, pipe: &str) -> bool {
    parent != 0
        && pipe
            .strip_prefix(&format!("snug-vdd-{parent}-"))
            .is_some_and(|nonce| {
                nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

pub(crate) fn launch(parent: u32, pipe: &str) -> io::Result<()> {
    if parent != std::process::id() || !valid_arguments(parent, pipe) {
        return Err(denied("驱动权限代理任务参数无效"));
    }
    let caller = current_identity()?;
    let session = i32::try_from(caller.session_id())
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| denied("驱动权限代理要求交互式 Windows 会话"))?;
    let _com = Com::enter()?;
    let (_, folder) = connect()?;
    let name = BSTR::from(task_name(caller.sid()));
    let task = scheduled_task(&folder, &name)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "尚未设置驱动后台权限"))?;
    verify_task(&task, caller.sid(), &helper_path(caller.sid())?)?;
    if !unsafe { task.Enabled() }.map_err(windows_error)?.as_bool() {
        return Err(denied("驱动后台权限已停用"));
    }
    let params = task_arguments(parent, pipe)?;
    unsafe { task.RunEx(&params, TASK_RUN_USE_SESSION_ID.0, session, &BSTR::new()) }
        .map_err(windows_error)?;
    Ok(())
}

fn task_arguments(parent: u32, pipe: &str) -> io::Result<VARIANT> {
    let array = unsafe { SafeArrayCreateVector(VT_BSTR, 0, 2) };
    if array.is_null() {
        return Err(io::Error::other("无法创建驱动任务参数"));
    }
    let arguments = Arguments(array);
    for (index, value) in [BSTR::from(parent.to_string()), BSTR::from(pipe)]
        .into_iter()
        .enumerate()
    {
        unsafe { SafeArrayPutElement(arguments.0, &(index as i32), value.as_ptr().cast()) }
            .map_err(windows_error)?;
    }
    let mut params = VARIANT::default();
    unsafe {
        let contents = &mut *params.Anonymous.Anonymous;
        contents.vt.0 = VT_ARRAY.0 | VT_BSTR.0;
        contents.Anonymous.parray = arguments.0;
    }
    // VARIANT owns the array from this point and frees it through VariantClear.
    std::mem::forget(arguments);
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_accepts_only_fixed_nonce_arguments() {
        let pipe = "snug-vdd-42-0123456789abcdef0123456789abcdef";
        assert!(valid_arguments(42, pipe));
        assert!(!valid_arguments(41, pipe));
        assert!(!valid_arguments(0, pipe));
        assert!(!valid_arguments(42, &format!("{pipe} --preview")));
        assert!(mapped_executable().is_ok());
        for _ in 0..4 {
            let arguments = task_arguments(42, pipe).unwrap();
            let copied = arguments.clone();
            assert_eq!(arguments.vt().0, VT_ARRAY.0 | VT_BSTR.0);
            drop(arguments);
            assert_eq!(copied.vt().0, VT_ARRAY.0 | VT_BSTR.0);
        }
    }

    #[test]
    fn protected_acl_rejects_inheritance_and_additional_writers() {
        let acl = task_acl("S-1-5-21-1-2-3-1001");
        require_acl(&SecurityDescriptor::parse(&acl).unwrap(), &acl).unwrap();
        let broadened = SecurityDescriptor::parse(&format!("{acl}(A;;FA;;;BU)")).unwrap();
        assert!(require_acl(&broadened, &acl).is_err());
        let inherited = SecurityDescriptor::parse(&acl.replace("D:P", "D:")).unwrap();
        assert!(require_acl(&inherited, &acl).is_err());
    }
}
