# 更新记录

版本号只在根 `Cargo.toml` 的 `[workspace.package] version` 写一处；
exe 资源版本、更新器比较用的版本都从这里来。

## 1.3.0 — 2026-11-19

对齐 C# 原版 **v1.3.0**（`c223cd4`）的功能面。原版的每一条改动都在
[`docs/upstream-watch.md`](docs/upstream-watch.md) 里有对照表和取舍理由。

### 遗留检查与体积优化

- 遗留窗口检查只取 PID 时，跳过逐进程的主窗口枚举；App 启动仍保留主窗口优先规则。
- `left-behind.json` 清掉过期项后同步保存，避免重启再次加载。仍保留 14 天、最多 20 条的规则。
- 建屏状态机接入现有 `create_named`，将当前配置的屏名传给驱动；legacy 协议的命名限制保持不变。
- release 默认改用 `opt-level = "s"`。内嵌驱动前，同一代码的 exe 从 6,999,552 B 降到 5,817,856 B，
  约缩小 17%；保留字体、图标、静态 CRT 和 unwind。
- 按单文件交付要求，将官方驱动 ZIP、带 BOM 的安装脚本和完整机型库嵌入 exe；
  自动安装与设置页安装共用内嵌包，正常结束后清理临时文件，无需旁边放 `driver\` 或 `assets\`。
- 管理员驱动脚本现在释放到 `System32` 下带受保护 DACL 的独占目录，普通权限进程不能改写安装内容。
- UU 新会话需覆盖层或可读控制端提供准入；本地游戏的 SLOG 增长不再单独触发开屏。
  已确认会话仍由心跳维持；隐藏且无准入信号时可用手动强制模式。
- 全仓 514 条单测通过，debug 和 release 配置均通过。微基准与未完成验收见
  [`docs/optimization-20261119.md`](docs/optimization-20261119.md)。

### 多屏

- `DeskSet` 多屏管理层：一条**共享串行队列**，每块屏一个 `DeskSlot`；
  跨屏搬窗（`DetachPlacementForTransfer`）、退出前归还别屏窗口、
  电脑屏疏散都在同一个动作里同步完成。
- 一块屏对一台设备，最多 4 块（`ScreenConfig::LIMIT`），按配置顺序启用；
  顶部屏切换 + 重入胶囊。
- 每块屏一份还原记录：`screen-<id>.json`（legacy 仍是 `phone-screen.json`）。
- 建屏按 `RegistrationOrder` 从宽到窄注册；候选 id 按已有虚拟屏数轮转，
  超时的候选不再误删。
- 虚拟屏带名字建屏（v2 协议支持；legacy v0.3.x 协议没有 name 字段，退回驱动默认名）。
- DPI 按 `ScreenFit::target_dpi_percent` 下发（电脑屏大尺寸走 100%）。
- 触控钩子支持多区域（`SetRegions` + 按按下点选活动区域）。
- `EvacuateComputerScreenWindows`：撤电脑屏前把落在它范围内的窗口搬回主屏居中。

### 界面

- 设置窗改成**控制中心**式首页：四张卡（状态 / 当前手机屏 / 外观 / 细项）
  + 手势 / 驱动日志 / 关于 / 多屏 四个页签；机型选择器内嵌。
- 主题 Light / Dark / Auto（`Auto` 读 `HKCU\…\Personalize\AppsUseLightTheme`）。
- 手机桌面式图标网格，横屏 6 列、竖屏 4 列（`UpdateResponsiveLayout`）。
- 机型选择器内嵌；支持**自定义分辨率**（C# `CustomModelSection`：没在搜索或搜不到时才露出来，
  宽高都要填，横向填的会自动摆成竖屏）与**添加屏时自定名字**（`AddScreenNameBox`，留空按机型取名）。
- 重入胶囊：手机屏收起后主屏右下角 260×52，点一下同一会话重进。
- 「⏏ 退出手机屏」入口 + 确认条，形成 收起 → 胶囊 → 重入 的闭环。
- 托盘：「预览手机界面…」「重新进入手机屏」「退出手机屏」。
- 桌面预览：没装驱动也能看界面，缩放走 `ScreenFit::compute_preview_layout`。
- 界面线程不再在建屏期间卡死：worker 发快照、界面只读快照，写操作排队。
- 未选中的页签看不见：`Button::selectable` 未选中时不画底板，改成显式底色 + 文字色。
- 搜索框打不出字：`WS_EX_NOACTIVATE` 被永久钉在界面窗口上，窗口永远拿不到键盘焦点。
  改成按需设置（只剩底部指示条时不抢焦点），与原版 `noActivate: !AcceptsKeys` 一致。
- 「运行中」页签不看搜索词：原版搜索是独立子页面，本版是内嵌输入框，两个页签都要过滤。

### 归属记录（与原版同机不再互拆）

- `OwnedDisplayBook` + `OwnedDisplayStore`：启动时只清理**自己登记的**虚拟屏
  （`owned-displays.json`），不再 `RemoveAll`；建屏上线后记账、撤屏成功后销账。
- 清理成功按原版文案记 `已按归属记录清理残留手机屏：#1, #2`。

### 机型库

- 换成 v1.3.0 的 `device-models.json`：**853 款**，含手机 / 平板 / 电脑，
  带 `IsTouch` / `UsualOrientation` / `AllowedOrientations` / `SwappableKinds`。
- `ScreenFit` 整块按 v1.3 重写：`FromDevice` 六条分支、按方向的 ratio 夹取与档位上限、
  `PixelSize` / `TargetDpiPercent` / `ComputePreviewLayout`；
  竖屏手机公式与 1.2 完全一致（老对拍 TSV 仍全过）。

### 修掉的原版同源问题

- 退出远控后虚拟屏不撤：每拍按会话重算 `SetWanted`（原版 `OnTick` 的语义）。
- 虚拟屏管道句柄缓存到进程结束：`PeekNamedPipe` 探测 + 写/读失败即丢弃重连；
  补上 `IPC Notify 失败` 日志。
- 会话每 7–20 秒自己断一次：在场判定回到 C# 语义（`capture_active = 推流日志增长`），
  把 `ADVANCE_PAIR_MAX_GAP` / `ACTIVE_HOLD_WINDOW` 从 3s / 2s 放到 15s——
  真机推流日志的节奏是操作时 2–3 秒、空闲 8–10 秒，原版那组阈值正好压在空闲 flush 周期上。
- 退出卡死数分钟：收尾的撤屏动作排在加屏重试阶梯（5/15/30 秒 × 每次建屏 8 秒 × 4 候选）后面。
  现在先在队列外取消重试，再带 20 秒上限等撤屏；超时留痕，残留屏由归属记录兜底清理。
- 点 App 要等几分钟：`check_returns` 每 500ms 入队、单次真机 3 秒，把共享队列堵死
  （`[lane] 动作排队 219.8s`）。改成去重 + 5 秒一次；根因是读窗口标题用了
  `SendMessageTimeoutW(WM_GETTEXT)`（708 个窗口累计 1499ms），换成 `GetWindowTextW`
  后 0.8ms，并按 pid 过滤窗口探测（`probes()` 1682ms → 5.3ms）。

### 铺开的响应速度

- 铺开窗口时屏幕被越推越宽（真机 850 → 910 → 970）：窗口最小高度跟着屏宽一起长，
  `WidthToFit` 每档都判"铺不满"。现在记住每个 App 最终用的屏宽（`learned-widths.json`，
  跨进程有效），并且宽度放得下、高度只溢出 <10% 时不再换档。
- 「找关闭按钮」（UIA，真机 0.6–1.5 秒）挪到铺好之后再做，不再挡住"窗口已铺好"的状态发布。
- 连点同一个 App 只认第一次：一次铺开要 1–5 秒，原先等待期间再点几下会排成好几条。

### 开机自启与驱动自动安装（真机走通）

- **开机自启本身就没坏过**：设置里的开关注册的是计划任务 `SnugWinAutoStart`
  （登录触发、`HighestAvailable`、参数 `--minimized`、工作目录=exe 目录）。
  真机复验：用产品自己的 `StartupService` 注册 → `schtasks /Query /XML` 字段全对 →
  `schtasks /Run` 拉起后进程正常常驻、写日志。之前那句「从任务启动会秒退、连日志都不写」
  是误判，已在文档里改成实测结论。
- `--minimized` 以前**没人解析**：自启和手动启动长得一模一样，窗口会先在主屏闪一下
  （实测创建在 `76,76`）。现在它真的管用——带这个参数启动时窗口先建在屏幕外
  （实测 `-32768,-32768`，不闪、不抢焦点），手机屏就绪后再由 `sync_window` 摆上去；
  升级重启（`launch_apply_script`）也沿用这个形态，不会因为一次升级变成弹窗。
- **管道探测会误判**：`Path::exists()` 在计划任务上下文里把"实例被占满"的管道答成不存在
  （同一台机器：任务里报"未检测到"，手动启动报"就绪"），后果是白跑一遍 msiexec 安装。
  现在用 `WaitNamedPipe(0)` 兜底：只有 `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`
  才算不存在，`ERROR_SEM_TIMEOUT`（实例全忙）和拒绝访问都算在。
- **自动安装端到端跑通**：卸掉 0.3.1 后冷启动 → 2.6 秒装完、6 秒内手机屏就绪。
  这一趟真机把只有"真装一次"才会暴露的问题挖了出来：
  1. 安装脚本是 UTF-8 **无 BOM**，而程序用 `powershell.exe`（5.1）拉它——5.1 按系统 ANSI
     读文件，中文注释直接把语法读崩（`ParserError: switch 语句缺少代码块`）。
     现在脚本带 BOM，并且 `build-release` / `preflight` 都会检查，缺了就报错不放行。
  2. 脚本用 `Get-FileHash` / `Expand-Archive`。从 PS7 父进程启动时子进程继承 PS7 的
     `PSModulePath`，5.1 会先去 PS7 的模块目录找 `Microsoft.PowerShell.Utility`（PS7 专用、
     加载不了），这些 cmdlet 全"不存在"。程序侧不再传 `PSModulePath`；脚本自己用 .NET
     算 SHA-256、用 `ZipFile` 解压，有 cmdlet 才退回 cmdlet。
  3. 脚本输出按控制台代码页（936）编码，收进日志全成替换符（`驱动：����λ��…`）。
     现在用 `-Command` 把子进程输出编码钉成 UTF-8，另加 ANSI/OEM 兜底解码。
  4. 脚本判定"驱动已装"是按显示名匹配 `*Virtual Display*`，把 UU 的
     `GameViewer Virtual Display Adapter` 也算成了自己。改成按 HardwareID
     `virtualdisplaydriver` 认。
- **登录早期的竞态**：开机自启那一下，驱动是装着的、但用户态服务还没把管道拉起来。
  原先"管道不在就装"会把驱动重装一遍（一次 msiexec 修复安装，慢且没意义）。
  现在先查注册表里有没有那块设备（`display::virtual_display_instance_id`）：
  装着就等 20 秒管道，等不到也不重装，交给建屏的重试阶梯。

### 体积与发布包

`snug.exe` **8.74 MiB**（9,164,800 B），已含驱动，单文件总预算 12 MiB。明细见
[`docs/size-budget.md`](docs/size-budget.md)。

自动更新 zip **5.74 MiB**（6,021,646 B），仅含同一个 `snug.exe`；直接交付用户只需裸 exe。
官方虚拟显示器驱动 ZIP（3,267,304 B）与安装脚本已内嵌。启动时缺少驱动就在后台
释放并安装，仍核对安装包、证书 SHA-256 与指纹；已经装过同款驱动则保留。
设置页的「安装驱动」按钮使用相同的内嵌包，正常结束后清理临时安装文件。

## 1.2.0 — 2026-11-18

首个可用的完整移植：几何 / 手势 / 配置 / 记录 / 窗口会话状态机 / 平台层 / egui 界面。
