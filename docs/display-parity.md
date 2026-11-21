# display 模块与 C# 版对拍摘要

范围：`crates/snug-win/src/display.rs` ↔ `references/ref-snug-win/src/SnugWin.Windows/Display/`
（`DisplayConfigService.cs` + `PhoneDisplayService.cs`）。只读核对，结论按方法逐条记录在
2026-11-18 的对拍报告里；这里留下需要长期记住的偏离与共同风险。

## 有意偏离 C#（保留）

| 项 | 内容 | 位置 |
|---|---|---|
| 稳定 ID | C# `HashCode.Combine` 是进程内随机化；我们用确定性 FNV-1a，跨进程稳定。**若将来持久化 display_id**，两版编号不通用，需与 C# 对齐或明确「不跨版本通用」 | `stable_id` |
| 查询加锁 | C# 的 PhoneDisplayService 是 async、等待时让出线程；Rust 无运行时，拆成「ScreenMaps 短锁 + ops 慢操作锁」。查询（界面每帧调 `home_bar_area`/`phone_bounds`）不经过 ops 锁，不会被 4s/8s/10s 的轮询挡住 | `WinPhoneDisplays` |
| CCD 重试 | **有意偏离 C#**：C# 的 `QueryDisplayConfig` 只查一次，热插拔窗口内会整次失败（所有屏 adapter/source 归零、DPI 掉回 100）。我们在 183/87 上重取 buffer sizes 重试 2 次，每次记 warn，仍失败返回空表 | `query_active_paths` |
| 舍入 | C# `Math.Round` 默认银行家舍入；Home Bar 像素、光标救援坐标、SetResolution 刷新率三处用 `round_ties_even`。`WidthOf` 兜底保持远离零（C# 那里明确 AwayFromZero） | `window_area` / `cursor_rescue_point` / `set_resolution` |
| 日志 | 补了 C# 的四处 Warn：SetResolution 返回码、建屏 8s 超时、撤屏 remove 失败、SetDpi 失败。查询路径不加日志 | 见源码 `crate::warn` |
| 取消 | C# 每个等待循环都有 CancellationToken；Rust `PhoneDisplays` 接口没有 token，退出时最长等 8s/4s/10s。**留债**，见 `impl PhoneDisplays for WinPhoneDisplays` 的 `TODO(取消)` | trait impl |

## 照抄不修的共同风险（C# 也一样，改了就产生行为差异）

1. **create**：notify 成功但 8s 未上线 → 尝试 remove；remove 失败会留孤儿屏（下次启动
   `CleanupOrphans` 才清）。
2. **create**：上线后 `SetWidth` 改分辨率失败 → 仍返回成功、fits 已记；屏在线但分辨率/DPI
   可能不对。
3. **reshape**：新模式 4s 内没出现 → 仍更新 fits 并返回 true（状态说新比例、实际可能旧比例）。
4. **retire**：先删本地状态再发删除；删除失败 → 屏还在但应用已忘记（孤儿屏）。
5. **try_attach**：`ChangeDisplaySettingsEx(CDS_NORESET)` 成功、紧接着 apply 失败 → 注册表里
   留下挂起模式；若随后的 `SetDisplayConfig(EXTEND)` 也失败，挂起变更留到下次。
6. **SetModeAsync**：C# 用 `StableId` 匹配 `displayId`；手机屏 id 是候选编号 0..3，
   理论上可能与真实屏的 StableId 撞号而改错屏。
7. **WaitForOnline（已收紧）**：验证 Cherry monitor interface、PnP 父适配器 HardwareID 与有效 EDID serial；
   serial 必须等于本次 connector ID，且 CCD source 唯一。不确定时跳过候选，不用分辨率或“第一个新屏”猜，
   避免把用户刚接入的实体显示器当成 Snug 屏。
   未上线屏的扩展路径同样使用 monitor interface 校验。此保护只覆盖建屏认领；
   运行中 `find_monitor_of` / `set_width` 仍依赖缓存的 GDI 名，用户随后切换克隆拓扑时的持续保护尚未完成。

## 与 C# 完全等价、但实现不同的地方

- `GetActiveMonitors` 先收 HMONITOR 再循环（顺序/过滤/默认值一致）。
- 字典用「小写键」代替 `StringComparer.OrdinalIgnoreCase`（设备名是 ASCII）。
- DPI 包（-3/-4、min/cur/max/scaleRel）与 C# 逐字段一致。
- 所有等待节奏（8/10/4s，150/250/100/300/200ms）一致。
- `TryAttachVirtualDisplay` 的 DEVMODE 字段、两次 ChangeDisplaySettingsEx 的 flags、
  失败后的 `SDC_APPLY|SDC_TOPOLOGY_EXTEND` 兜底一致。
- `MainArea`/`OtherDisplays`/`WindowAreaOf`/`HomeBarAreaOf`/`WidthOf` 的公式与过滤一致。
