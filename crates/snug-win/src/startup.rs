//! 开机自启动（计划任务），移植自 C# 版 `SnugWin.Windows.Support.StartupTaskService`。
//!
//! 用 `schtasks.exe` 而不是 COM 的 Task Scheduler：少一整套 COM 接口，
//! 任务定义还是走 C# 原来那份 XML（`snug_core::update::build_task_xml`），
//! 登录触发 + `--minimized` 参数不变，GUI 使用最低权限，不再随登录自动提升。
//!
//! 四件事：查（`/Query`）、装（`/Create /XML`）、删（`/Delete /F`）、
//! 以及"程序被挪了位置就把任务里的路径改过来"（`RefreshPathIfEnabled`）。
//!
//! 所有外部命令都经 [`CommandRunner`]，单测用假实现，**不会真的在本机建任务**。
//!
//! 手动验证（需要时在真机上手动跑）：
//! 1. 打开"任务计划程序"，应能看到名为 `SnugWinAutoStart` 的任务，触发器为"登录时"，
//!    "使用最高权限运行"未勾选，操作为 `snug.exe --minimized`；
//! 2. 关掉自启后再打开，任务应被删掉；
//! 3. 把 exe 换个目录启动，任务里的路径应自动跟着变。

use std::fmt;
use std::path::{Path, PathBuf};

use snug_core::update::{build_task_xml, TASK_NAME};

/// 计划任务命令。
pub const SCHTASKS: &str = "schtasks.exe";

/// 跑外部命令的结果（对应 C# 里 `(int ExitCode, string Output)`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: i32,
    /// 优先 stderr，空则 stdout（C# 的取法）。
    pub output: String,
}

impl CommandOutput {
    pub fn new(code: i32, stdout: &str, stderr: &str) -> Self {
        let output = if stderr.trim().is_empty() { stdout } else { stderr };
        Self { code, output: output.trim().to_string() }
    }
}

/// 执行命令的抽象。真机是 [`SystemRunner`]，测试里是记录调用的假实现。
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput>;
}

/// 真机：起进程、抓输出、不闪黑框。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        /// 不弹命令行窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let executable = crate::system_paths::system_tool(program)?;
        let child = Command::new(executable)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            code: output.status.code().unwrap_or(-1),
            output: {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() }
            },
        })
    }
}

/// 自启相关的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupError {
    /// 找不到当前可执行文件，没法注册。
    ExeMissing(String),
    /// `/Create` 失败。
    CreateFailed { code: i32, output: String },
    /// `/Delete` 失败（而且任务还在）。
    DeleteFailed { output: String },
    /// 连进程都起不来。
    Spawn(String),
    /// 写临时 XML 失败。
    Io(String),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExeMissing(path) => {
                write!(f, "无法确定当前可执行文件路径（{path}）")
            }
            Self::CreateFailed { code, output } => {
                write!(f, "注册开机自启动任务失败（退出码 {code}）：{output}")
            }
            Self::DeleteFailed { output } => {
                write!(f, "删除开机自启动任务失败：{output}")
            }
            Self::Spawn(message) => write!(f, "无法启动 schtasks.exe：{message}"),
            Self::Io(message) => write!(f, "读写自启动任务文件失败：{message}"),
        }
    }
}

impl std::error::Error for StartupError {}

// ---------------------------------------------------------------------------
// 命令行参数（纯逻辑，单测直接比对）
// ---------------------------------------------------------------------------

/// `schtasks /Query /TN <name>`。
pub fn query_args(task: &str) -> Vec<String> {
    vec!["/Query".to_string(), "/TN".to_string(), task.to_string()]
}

/// `schtasks /Query /TN <name> /XML`。
pub fn query_xml_args(task: &str) -> Vec<String> {
    vec!["/Query".to_string(), "/TN".to_string(), task.to_string(), "/XML".to_string()]
}

/// `schtasks /Create /TN <name> /XML <path> /F`。
pub fn create_args(task: &str, xml_path: &Path) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/TN".to_string(),
        task.to_string(),
        "/XML".to_string(),
        xml_path.to_string_lossy().to_string(),
        "/F".to_string(),
    ]
}

/// `schtasks /Delete /TN <name> /F`。
pub fn delete_args(task: &str) -> Vec<String> {
    vec!["/Delete".to_string(), "/TN".to_string(), task.to_string(), "/F".to_string()]
}

/// 从 `/Query /XML` 的输出里取 `<Command>`（C# 用正则 + `WebUtility.HtmlDecode`）。
pub fn parse_task_command(xml: &str) -> Option<String> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = xml[start..].find("</Command>")? + start;
    let command = html_decode(xml[start..end].trim());
    if command.trim().is_empty() { None } else { Some(command.trim().to_string()) }
}

/// `WebUtility.HtmlDecode` 的等价物：命名实体 + 十进制/十六进制数字实体。
///
/// 只处理 XML 里会出现的这几个；不认识的实体原样保留。
pub fn html_decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let Some(semicolon) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            other => {
                if let Some(hex) = other.strip_prefix("#x").or_else(|| other.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = other.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(ch) => out.push(ch),
            None => out.push_str(&tail[..=semicolon]),
        }
        rest = &tail[semicolon + 1..];
    }
    out.push_str(rest);
    out
}

/// 已注册的路径和当前 exe 不一样（忽略大小写）就需要重新注册。
pub fn needs_refresh(registered: &str, exe_path: &str) -> bool {
    !registered.eq_ignore_ascii_case(exe_path)
}

/// 写计划任务 XML 用的字节：UTF-16LE + BOM（C# 用 `Encoding.Unicode`）。
pub fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

// ---------------------------------------------------------------------------
// 服务本体
// ---------------------------------------------------------------------------

/// 自启任务服务（C# `StartupTaskService`，静态类 → 这里做成可注入的对象）。
pub struct StartupService<R: CommandRunner> {
    runner: R,
    task: String,
    /// 当前的 exe 路径。
    exe_path: PathBuf,
    /// 放临时 XML 的目录。
    temp_dir: PathBuf,
    protected_staging: bool,
}

struct TaskXml {
    path: PathBuf,
    protected_directory: Option<PathBuf>,
}

impl Drop for TaskXml {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(directory) = &self.protected_directory {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

impl StartupService<SystemRunner> {
    /// 用当前进程的可执行文件。
    pub fn for_current_exe() -> Self {
        let exe = std::env::current_exe().unwrap_or_default();
        let mut service = Self::new(SystemRunner, TASK_NAME, exe, std::env::temp_dir());
        service.protected_staging = crate::elevation::is_elevated();
        service
    }
}

impl<R: CommandRunner> StartupService<R> {
    pub fn new(runner: R, task: &str, exe_path: impl Into<PathBuf>, temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            task: task.to_string(),
            exe_path: exe_path.into(),
            temp_dir: temp_dir.into(),
            protected_staging: false,
        }
    }

    pub fn task_name(&self) -> &str {
        &self.task
    }

    /// 任务在不在（C# `IsEnabled`）。
    pub fn is_enabled(&self) -> bool {
        self.run(query_args(&self.task)).map(|out| out.code == 0).unwrap_or(false)
    }

    /// 开/关自启（C# `SetEnabled`）。
    pub fn set_enabled(&self, enabled: bool) -> Result<(), StartupError> {
        if !enabled {
            let output = self.run(delete_args(&self.task))?;
            if output.code != 0 && self.is_enabled() {
                return Err(StartupError::DeleteFailed { output: output.output });
            }
            return Ok(());
        }

        let exe = self.exe_path.to_string_lossy().to_string();
        if exe.is_empty() || !self.exe_path.exists() {
            return Err(StartupError::ExeMissing(exe));
        }
        let working_directory = self
            .exe_path
            .parent()
            .map(|dir| dir.to_string_lossy().to_string());
        let xml = build_task_xml(&exe, working_directory.as_deref());

        let temporary = if self.protected_staging {
            let file_name = self.temp_xml_path().file_stem().unwrap_or_default().to_os_string();
            let directory = crate::system_paths::system_directory().map_err(|error| StartupError::Io(error.to_string()))?
                .join(file_name);
            crate::system_paths::create_protected_directory(&directory).map_err(|error| StartupError::Io(error.to_string()))?;
            TaskXml { path: directory.join("task.xml"), protected_directory: Some(directory) }
        } else {
            TaskXml { path: self.temp_xml_path(), protected_directory: None }
        };
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary.path)
            .map_err(|error| StartupError::Io(error.to_string()))?;
        file.write_all(&utf16le_with_bom(&xml)).map_err(|error| StartupError::Io(error.to_string()))?;
        drop(file);

        let result = self.run(create_args(&self.task, &temporary.path));
        drop(temporary);

        let output = result?;
        if output.code != 0 {
            return Err(StartupError::CreateFailed { code: output.code, output: output.output });
        }
        Ok(())
    }

    /// 程序换位置了就把任务里的路径改过来（C# `RefreshPathIfEnabled`）。
    ///
    /// 返回 `true` 表示确实重新注册了。
    pub fn refresh_path_if_enabled(&self) -> bool {
        let Ok(output) = self.run(query_xml_args(&self.task)) else {
            return false;
        };
        if output.code != 0 || output.output.trim().is_empty() {
            return false;
        }
        let Some(registered) = parse_task_command(&output.output) else {
            return false;
        };
        let exe = self.exe_path.to_string_lossy().to_string();
        let legacy_elevation = output.output.contains("<RunLevel>HighestAvailable</RunLevel>");
        if exe.is_empty() || (!needs_refresh(&registered, &exe) && !legacy_elevation) {
            return false;
        }
        match self.set_enabled(true) {
            Ok(()) => true,
            Err(error) => { crate::warn(&format!("无法迁移开机任务到普通权限；若旧任务拒绝修改，请在设置中以管理员重启一次，迁移后退出并普通启动：{error}")); false }
        }
    }

    fn run(&self, args: Vec<String>) -> Result<CommandOutput, StartupError> {
        self.runner
            .run(SCHTASKS, &args)
            .map_err(|error| StartupError::Spawn(error.to_string()))
    }

    /// UTF-16 的临时文件（`.xml` 扩展名不变，schtasks 靠内容认）。
    fn temp_xml_path(&self) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        self.temp_dir.join(format!("snugwin-autostart-{unique:032x}.xml"))
    }
}

/// 便于别处只想知道"能不能注册"（比如设置界面禁用开关）。
pub fn current_exe_usable() -> bool {
    std::env::current_exe().map(|path| path.exists()).unwrap_or(false)
}

/// 让调用方别在热路径里用系统时间做唯一名（测试与文档用）。
pub const TEMP_PREFIX: &str = "snugwin-autostart-";

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        /// 依次返回的结果；用完之后一律返回成功。
        responses: Mutex<std::collections::VecDeque<CommandOutput>>,
    }

    impl FakeRunner {
        fn with(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput> {
            self.calls.lock().unwrap().push((program.to_string(), args.to_vec()));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(CommandOutput { code: 0, output: String::new() }))
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snug-rs-startup-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn argument_lists_match_csharp() {
        assert_eq!(query_args("T"), vec!["/Query", "/TN", "T"]);
        assert_eq!(query_xml_args("T"), vec!["/Query", "/TN", "T", "/XML"]);
        assert_eq!(
            create_args("T", Path::new(r"C:\tmp\a.xml")),
            vec!["/Create", "/TN", "T", "/XML", r"C:\tmp\a.xml", "/F"]
        );
        assert_eq!(delete_args("T"), vec!["/Delete", "/TN", "T", "/F"]);
    }

    #[test]
    fn task_xml_is_written_as_utf16_with_bom() {
        let bytes = utf16le_with_bom("<a/>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert_eq!(&bytes[2..], "<a/>".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    }

    #[test]
    fn command_tag_is_decoded() {
        let xml = r#"<Task><Actions><Exec><Command>C:\Tools\Snug &amp; Win\SnugWin.App.exe</Command><Arguments>--minimized</Arguments></Exec></Actions></Task>"#;
        assert_eq!(
            parse_task_command(xml).as_deref(),
            Some(r"C:\Tools\Snug & Win\SnugWin.App.exe")
        );
        assert_eq!(parse_task_command("<Task/>"), None);
        assert_eq!(parse_task_command("<Command>   </Command>"), None);
    }

    #[test]
    fn html_decode_handles_named_and_numeric_entities() {
        assert_eq!(html_decode("a &amp; b"), "a & b");
        assert_eq!(html_decode("&lt;x&gt;"), "<x>");
        assert_eq!(html_decode("&quot;q&quot; &apos;s&apos;"), "\"q\" 's'");
        assert_eq!(html_decode("&#65;&#x42;"), "AB");
        // 不认识的实体、没有分号的 & 原样保留。
        assert_eq!(html_decode("&unknown; & tail"), "&unknown; & tail");
    }

    #[test]
    fn refresh_needs_a_different_path() {
        assert!(!needs_refresh(r"C:\a\app.exe", r"C:\A\APP.EXE"));
        assert!(needs_refresh(r"C:\a\app.exe", r"C:\b\app.exe"));
    }

    #[test]
    fn is_enabled_reads_the_exit_code() {
        let enabled = FakeRunner::with(vec![CommandOutput::new(0, "ok", "")]);
        let service = StartupService::new(enabled, "T", r"C:\a\app.exe", temp_dir());
        assert!(service.is_enabled());
        assert_eq!(service.task_name(), "T");

        let disabled = FakeRunner::with(vec![CommandOutput::new(1, "", "找不到任务")]);
        let service = StartupService::new(disabled, "T", r"C:\a\app.exe", temp_dir());
        assert!(!service.is_enabled());
    }

    #[test]
    fn enabling_writes_the_xml_and_calls_create() {
        let dir = temp_dir();
        let exe = dir.join("SnugWin.App.exe");
        std::fs::write(&exe, b"").unwrap();

        let runner = FakeRunner::default();
        let service = StartupService::new(runner, TASK_NAME, &exe, &dir);
        service.set_enabled(true).expect("应能注册");

        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            !entries.iter().any(|name| name.starts_with(TEMP_PREFIX)),
            "临时 XML 用完要删掉：{entries:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_a_task_passes_the_expected_arguments() {
        let dir = temp_dir();
        let exe = dir.join("SnugWin.App.exe");
        std::fs::write(&exe, b"").unwrap();

        let runner = std::sync::Arc::new(FakeRunner::default());
        let service = StartupService::new(SharedRunner(runner.clone()), TASK_NAME, &exe, &dir);
        service.set_enabled(true).expect("应能注册");

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, SCHTASKS);
        assert_eq!(calls[0].1[0], "/Create");
        assert_eq!(calls[0].1[1], "/TN");
        assert_eq!(calls[0].1[2], TASK_NAME);
        assert_eq!(calls[0].1[3], "/XML");
        assert!(calls[0].1[4].starts_with(&dir.to_string_lossy().to_string()));
        assert_eq!(calls[0].1[5], "/F");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 包一层 `Arc`，让测试在注册之后还能读调用记录。
    struct SharedRunner(std::sync::Arc<FakeRunner>);

    impl CommandRunner for SharedRunner {
        fn run(&self, program: &str, args: &[String]) -> std::io::Result<CommandOutput> {
            self.0.run(program, args)
        }
    }

    #[test]
    fn enabling_without_an_exe_fails_fast() {
        let runner = FakeRunner::default();
        let service = StartupService::new(runner, TASK_NAME, r"C:\nope\missing.exe", temp_dir());
        let error = service.set_enabled(true).expect_err("没有 exe 不能注册");
        assert_eq!(error, StartupError::ExeMissing(r"C:\nope\missing.exe".to_string()));
        assert!(error.to_string().contains("无法确定当前可执行文件路径"));
    }

    #[test]
    fn create_failure_reports_the_exit_code_and_output() {
        let dir = temp_dir();
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"").unwrap();

        let runner = FakeRunner::with(vec![CommandOutput::new(5, "", "拒绝访问")]);
        let service = StartupService::new(runner, TASK_NAME, &exe, &dir);
        let error = service.set_enabled(true).expect_err("创建失败要报错");
        assert_eq!(
            error,
            StartupError::CreateFailed { code: 5, output: "拒绝访问".to_string() }
        );
        assert!(error.to_string().contains("退出码 5"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabling_deletes_and_tolerates_a_missing_task() {
        // 删除失败但查询也说"不在" → 当作成功（C# 同样判断）。
        let runner = FakeRunner::with(vec![
            CommandOutput::new(1, "", "任务不存在"),
            CommandOutput::new(1, "", "任务不存在"),
        ]);
        let service = StartupService::new(SharedRunner(std::sync::Arc::new(runner)), TASK_NAME, r"C:\a\app.exe", temp_dir());
        assert!(service.set_enabled(false).is_ok());
    }

    #[test]
    fn disabling_reports_a_real_failure() {
        let runner = FakeRunner::with(vec![
            CommandOutput::new(1, "", "拒绝访问"),
            CommandOutput::new(0, "任务还在", ""),
        ]);
        let service = StartupService::new(runner, TASK_NAME, r"C:\a\app.exe", temp_dir());
        let error = service.set_enabled(false).expect_err("删不掉就要报错");
        assert_eq!(error, StartupError::DeleteFailed { output: "拒绝访问".to_string() });
    }

    #[test]
    fn refresh_re_registers_only_when_the_path_changed() {
        let dir = temp_dir();
        let exe = dir.join("SnugWin.App.exe");
        std::fs::write(&exe, b"").unwrap();

        let xml_same = format!(
            "<Task><Actions><Exec><Command>{}</Command></Exec></Actions></Task>",
            exe.to_string_lossy()
        );
        let runner = std::sync::Arc::new(FakeRunner::with(vec![CommandOutput::new(0, &xml_same, "")]));
        let service = StartupService::new(SharedRunner(runner.clone()), TASK_NAME, &exe, &dir);
        assert!(!service.refresh_path_if_enabled(), "路径没变不该重注册");
        assert_eq!(runner.calls().len(), 1);

        let xml_moved = "<Task><Actions><Exec><Command>C:\\old\\SnugWin.App.exe</Command></Exec></Actions></Task>";
        let runner = std::sync::Arc::new(FakeRunner::with(vec![CommandOutput::new(0, xml_moved, "")]));
        let service = StartupService::new(SharedRunner(runner.clone()), TASK_NAME, &exe, &dir);
        assert!(service.refresh_path_if_enabled(), "路径变了要重注册");
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1[0], "/Create");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_copes_with_unreadable_task_xml() {
        let runner = FakeRunner::with(vec![CommandOutput::new(1, "", "找不到")]);
        let service = StartupService::new(runner, TASK_NAME, r"C:\a\app.exe", temp_dir());
        assert!(!service.refresh_path_if_enabled());

        let runner = FakeRunner::with(vec![CommandOutput::new(0, "<Task/>", "")]);
        let service = StartupService::new(runner, TASK_NAME, r"C:\a\app.exe", temp_dir());
        assert!(!service.refresh_path_if_enabled(), "没有 <Command> 就什么都不做");
    }

    #[test]
    fn refresh_migrates_legacy_elevated_tasks_even_if_the_path_is_unchanged() {
        let directory = temp_dir();
        let exe = directory.join("snug.exe");
        std::fs::write(&exe, b"test").unwrap();
        let xml = format!("<Task><RunLevel>HighestAvailable</RunLevel><Command>{}</Command></Task>", exe.display());
        let runner = std::sync::Arc::new(FakeRunner::with(vec![CommandOutput::new(0, &xml, "")]));
        let service = StartupService::new(SharedRunner(runner.clone()), TASK_NAME, &exe, &directory);
        assert!(service.refresh_path_if_enabled());
        assert_eq!(runner.calls()[1].1[0], "/Create");
        std::fs::remove_file(exe).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn current_exe_is_reported_for_the_ui() {
        assert!(current_exe_usable(), "测试进程自己总该有 exe");
        assert_eq!(TEMP_PREFIX, "snugwin-autostart-");
    }
}
