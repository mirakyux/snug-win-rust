# 更新记录

版本号只在根 `Cargo.toml` 的 `[workspace.package] version` 写一处；
exe 资源版本、更新器比较用的版本都从这里来。

## 1.3.0 — 2026-11-19

对齐 C# 原版 **v1.3.0**（`c223cd4`）的功能面。原版的每一条改动都在
[`docs/upstream-watch.md`](docs/upstream-watch.md) 里有对照表和取舍理由。

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

### 体积与发布包

`snug.exe` **6.64 MiB**（6,963,200 B），上限 8 MiB。明细见
[`docs/size-budget.md`](docs/size-budget.md)。

发布 zip **6.19 MiB**，里面是 `snug.exe` + `driver\`（官方虚拟显示器驱动安装包 3.27 MB
与安装脚本）。驱动跟着包走、而且**自动装**：启动时探不到
`\\.\pipe\virtualdisplaydriver` 就在后台跑安装脚本（核对安装包与证书 SHA-256），
装完直接建屏；已经装过同款驱动则原样保留。设置页的「安装驱动」按钮留作修复用。

## 1.2.0 — 2026-11-18

首个可用的完整移植：几何 / 手势 / 配置 / 记录 / 窗口会话状态机 / 平台层 / egui 界面。
