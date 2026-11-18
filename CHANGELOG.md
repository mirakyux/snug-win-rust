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
- 重入胶囊：手机屏收起后主屏右下角 260×52，点一下同一会话重进。
- 「⏏ 退出手机屏」入口 + 确认条，形成 收起 → 胶囊 → 重入 的闭环。
- 托盘：「预览手机界面…」「重新进入手机屏」「退出手机屏」。
- 桌面预览：没装驱动也能看界面，缩放走 `ScreenFit::compute_preview_layout`。
- 界面线程不再在建屏期间卡死：worker 发快照、界面只读快照，写操作排队。

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
- UU 在场判定：推流日志增长只作诊断，判在场需要对端 / 覆盖窗 / 控制端佐证
  （原版 `<= 3.0s` 阈值正好压在空闲 flush 周期上，会造成屏每隔几十秒自己弹出）。

### 体积

`snug.exe` **6.59 MiB**（6,905,344 B），上限 8 MiB。明细见
[`docs/size-budget.md`](docs/size-budget.md)。

## 1.2.0 — 2026-11-18

首个可用的完整移植：几何 / 手势 / 配置 / 记录 / 窗口会话状态机 / 平台层 / egui 界面。
