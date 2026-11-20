# 真机联调步骤（Rust 版 vs 已装的 C# 正式版）

这台机器上同时装着 C# 正式版 `C:\Program Files\SnugWin` 和 Rust 版重写（现在两边都是 1.3 功能面）。
两边是**同一个产品**，会抢同几样东西：

- 数据目录 `%LOCALAPPDATA%\SnugWin`（`config.json`、`phone-screen.json` / `screen-<id>.json`、
  `zoom.json`、`left-behind.json`、`owned-displays.json`、`logs\`）
- 单实例锁 `%LOCALAPPDATA%\SnugWin\snug.lock`
- 虚拟显示器驱动管道 `\\.\pipe\virtualdisplaydriver` 和"虚拟屏"这块显示设备

所以联调时**必须**做两件事：先退掉 C# 版，再给 Rust 版一个独立数据目录。

> 1.3 起两边同机不再互拆屏幕：各自只清理 `owned-displays.json` 里自己登记的虚拟屏
> （`docs/upstream-watch.md` 的"归属记录"一条）。但配置文件和单实例锁仍然共用，
> 所以独立数据目录这件事没变。

## 阶段 0：静态门禁（不启动程序）

```powershell
pwsh -File scripts\preflight.ps1 -Exe target\release-check\release\snug.exe
```

版本、体积、清单、依赖、发行包一次查完。全 PASS 再往下；行为验收照 `docs/acceptance.md` 八组。

## 阶段 A：安全启动（不碰显示配置）

目的：确认托盘、设置窗、单实例、日志、窗口搬运代码在真实环境不炸。
**这一步不会建虚拟屏**，因为配置里 `phone_screen` 先关掉。

```powershell
# 1. 独立数据目录，配一份 phone_screen=false 的初始配置
$testData = "$env:USERPROFILE\snug-rs-test"
New-Item -ItemType Directory -Force "$testData\logs" | Out-Null
'{"phoneScreen":false}' | Set-Content "$testData\config.json" -Encoding utf8

# 2. 从普通权限终端启动（manifest 为 asInvoker）
$env:SNUGWIN_HOME = $testData
.\target\release\snug.exe
```

要看到的：

- 托盘出现 `snugwin-idle.ico`，提示文字 `SnugWin - 手机屏：关`
- 双击托盘 / 右键"打开设置与诊断..." → 设置窗五个页签可翻（控制中心 / 手势与交互 / 驱动与日志 / 关于与更新 / 多屏与设备）
- 设置窗"驱动与日志"页显示管道就绪（这台机器驱动已装）
- `$testData\logs\snug-YYYYMMDD.log` 里有启动行、数据目录行、托盘就绪行
- 关闭设置窗、托盘"退出 SnugWin（先还原窗口）"能干净退出，托盘图标消失

退出后检查：`%LOCALAPPDATA%\SnugWin\config.json` **时间戳没变**（证明没串到正式版目录）。

## 阶段 B：真接管手机屏（会建虚拟屏，动显示配置）

前提：C# 正式版已经退出（托盘右键退出），否则两边会抢同一个驱动管道。
GUI 保持普通权限；驱动管道拒绝普通权限访问时，首次真实建屏会请求受限 VDD 代理的 UAC。
取消或代理失败后，本次 GUI 生命周期内不再反复请求 UAC；修正问题后重启 GUI 再试。

```powershell
Stop-Process -Name SnugWin.App -ErrorAction SilentlyContinue
$env:SNUGWIN_HOME = "$env:USERPROFILE\snug-rs-test"
# 把 phone_screen 改回 true，或直接在设置窗里开"手机屏"
.\target\release\snug.exe
```

要看到的（按日志顺序）：

1. `desk:` 行出现建屏请求，`虚拟屏驱动管道：就绪`
2. 手机屏阶段 `Preparing` → `Ready`，日志 `手机屏 #N 已就绪（W 宽）`，`display_id` 有值
3. `手机屏界面：铺满资源库，搬到 (x, y) W×H` —— 我们自己的 egui 窗口被 `SetWindowPos` 搬到了虚拟屏上
   （收成底部条时是 `收成底部指示条`，预览窗是 `预览窗：预览（只读），搬到 …`）
4. Windows 显示设置里多出 `Virtual Display` 一块，分辨率符合机型比例
5. App 资源库里能列出真实运行中的窗口

退出时检查归位：被搬到手机屏上的窗口回到原屏幕原位，虚拟屏撤掉，`phone-screen.json` 记下状态。

### 退出验收（必须逐项过）

放一个退出请求，让它自己走完收尾（撤托盘、撤屏、归位窗口、存配置）：

```powershell
Set-Content "$env:SNUGWIN_HOME\quit.request" -Value quit -Encoding ascii
```

判 PASS 的四条：

1. **30 秒内进程退出，退出码 0**。卡住不动的，看下面「已知陷阱」第一条。
2. 日志按顺序走完：`收到退出请求文件，开始收尾` → `收尾：开始（撤托盘 + 撤屏 + 归位窗口 + 存配置）`
   → `已撤掉手机屏 #N` 与铺上去的窗口 `… 已放回原位` → `收尾：完成`。
3. `Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorBasicDisplayParams` 的条数
   **回到建屏之前的数量**——虚拟屏真的撤掉了，没留孤儿屏。
4. `Get-Process -Name snug` 为空。

第 2 步停在半路、进程还活着，说明是**收尾阻塞**，不是"慢"，别等，按陷阱清单查。

## 已知陷阱

### 收尾时读窗口标题会自己等自己（已修，`dee0c10`）

`GetWindowTextW` 对**同进程**窗口是同步投递 `WM_GETTEXT` 给对方窗口所属的线程。收尾时
UI 线程阻塞等 desk 线程撤屏，而 desk 线程正在做窗口快照（`EnumWindows` → 逐个读标题），
筛出来的正是我们自己的 egui 窗口 —— 两个线程互等，进程永远退不出去，虚拟屏留在桌面上。
日志现象是停在 `desk: 撤屏前别的屏：…` 之后一行都没有。

现在 `window_title` 改用 `SendMessageTimeoutW(WM_GETTEXT, 150ms)`，读不到返回空串，并且
`probe_window` 直接跳过本进程的窗口。**以后再加"枚举窗口顺便读标题"的代码，不要回头用
`GetWindowTextW` / `GetWindowTextLengthW`。**

### 启动方式：脚本明确使用 CreateProcess

旧版 `requireAdministrator` 清单下，通过 ShellExecute 启动曾出现安全警告或等待提权的现象。
当前 GUI 已改为 `asInvoker`；`scripts/smoke-test.ps1` 使用
`ProcessStartInfo(UseShellExecute=$false)` 明确继承普通权限。手工测试可在普通权限
PowerShell 里直接执行 `.\target\release\snug.exe`。

## 开机自启（计划任务）怎么验

```powershell
# 1. 注册/核对：设置里的开关调的就是这一段（StartupService → schtasks /Create /XML）
schtasks /Query /TN SnugWinAutoStart /XML |
    Select-String '<Command>|<Arguments>|<WorkingDirectory>|RunLevel'
#    期望：Command=当前 exe、Arguments=--minimized、WorkingDirectory=exe 目录、RunLevel=LeastPrivilege

# 2. 立刻跑一次（等价于"登录时触发"）
schtasks /Run /TN SnugWinAutoStart

# 3. 日志里应当有两行
#    [Info] 启动形态：开机自启（--minimized，不闪窗、不抢焦点）
#    [Info] 虚拟屏驱动管道：就绪
```

历史实测（2026-11-19，旧版权限策略）：**任务调度器直启这个 exe 是好用的**，登录触发 / 最高权限 /
工作目录都对，起来后正常常驻、正常写日志。带 `--minimized` 时窗口创建在屏幕外
（实测 `-32768,-32768`），不带时创建在 `76,76`（自启时会闪一下，这就是这个参数的意义）。
此记录不代表当前最低权限任务已完成登录实测；当前版本还需按上面步骤重新验收。

> 以前这里写过"从任务启动会秒退、连日志都不写"，那是**误判**，别再照着查。
> 真正容易看错的是单实例：同一个数据目录下已有实例时，任务拉起的进程会按单实例锁
> 正常退出（日志 `已有实例在运行，本次退出`），看起来就像"没起来"。

## 驱动自动安装怎么验

```powershell
# 卸掉本机的官方 0.3.1（本机装的就是它）
Start-Process msiexec.exe -Wait -ArgumentList '/x "{605D17E8-32FF-4B09-9247-122A1AACFAAD}" /passive /norestart'
[IO.File]::Exists('\\.\pipe\virtualdisplaydriver')   # 应当 False

# 冷启动程序，日志应当依次出现：
#   驱动：检测到虚拟显示器驱动未就绪，自动安装（…\driver\Install-SnugDriver.ps1）
#   驱动：[OK] SHA256 & 证书指纹校验通过 (5DFA…)
#   驱动：[2/3] 安装 virtual-display-driver-0.3.1-x86_64.msi...
#   驱动：命名管道已就绪，可以建手机屏了
#   desk: 手机屏 #0 已就绪（540 宽）
```

真机耗时：脚本 2.6 秒装完，冷启动到手机屏就绪约 6 秒。

**四个坑（都已在代码里修掉；改脚本或改调用方式前先读一遍）：**

1. `driver\Install-SnugDriver.ps1` 必须 **UTF-8 with BOM**。程序用 `powershell.exe`（5.1）
   拉它，5.1 对没有 BOM 的文件按系统 ANSI 代码页读，中文注释会把语法读崩
   （`ParserError: switch 语句缺少代码块`）。`build-release.ps1` / `preflight.ps1` 都加了
   检查，缺 BOM 直接失败——**用编辑工具改完这个脚本后要重新存成带 BOM 的 UTF-8**。
2. 拉起 `powershell.exe` 时**不要**把父进程的 `PSModulePath` 传下去。父进程若是 PS7，
   5.1 会去 PS7 的模块目录找 `Microsoft.PowerShell.Utility` 并失败，`Get-FileHash` 之类
   cmdlet 全部"不存在"。驱动入口现在把模块路径固定到系统 PowerShell，启动后再用
   `$PSHOME\Modules` 收紧，同时使用受保护暂存目录作为管理员子进程的工作目录。
3. 子进程输出是控制台代码页（简中 936），按 UTF-8 读进日志全是替换符。
   现在用 `-Command` 先把 `[Console]::OutputEncoding` 钉成 UTF-8 再跑脚本。
4. 判断"驱动已装"必须按 HardwareID `virtualdisplaydriver`，不能按显示名——
   UU 的 `GameViewer Virtual Display Adapter` 名字里也有 "Virtual Display"，会被误认成自己。

## 出问题看哪里
- 平台层告警走 `snug_win::set_warn_sink` → 应用日志的 `[Warn]`，不再只进 stderr。
- 建屏/撤屏失败会按 `5 / 15 / 30` 秒重试三次（C# 同）；日志里能看到每次尝试。
- 手机屏最短存活 2.5 秒、断开防抖 1.5 秒 + 静默期 5 秒、重连窗口 15 秒——这些是照抄 C# 的时序，
  联调时如果看到"刚撤又建""建了马上撤"，先对照这几个数，不要先怀疑代码写错。
