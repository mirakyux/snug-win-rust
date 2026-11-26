# 体积预算

单文件、零运行时依赖是本项目的一条硬要求（C# 原版要装 .NET 运行时）。
原版发版包也就几 MB，我们**不能**变成几十 MB 的那种"Rust 大块头"。

## 当前实测

| 项 | 大小 | 时间 |
| --- | --- | --- |
| `dist/snug.exe`（字体 XPRESS_HUFF、预生成托盘 RGBA，`opt-level = "s"`） | 8,862,720 B（8.45 MiB） | 2026-11-21 |
| 基线 `dist/snug.exe`（字体原样、ICO 运行时解码，`opt-level = "s"`） | 9,377,280 B（8.94 MiB） | 2026-11-20 |
| `dist/Snug-v1.3.0-win-x64-selfcontained.zip`（本轮） | 6,193,339 B（5.91 MiB） | 2026-11-21 |

用户要求仅交付一个内含驱动的 exe。驱动安装包 3,267,304 B 和安装脚本直接内嵌，
正常安装结束后清理临时释放的文件；不再要求随附 `driver\` 目录。
自动更新 zip 仅包含相同的 `snug.exe`，保留 zip 是为了兼容更新器。

历史：5.09 MiB → 6.13 MiB（加托盘、截图、UU 探测、App 资源库）
→ 6.26 MiB（修 bug）→ 6.56 MiB（多屏 `DeskSet`、主题、控制中心设置页、重入胶囊）
→ 6.59 MiB（退出手机屏确认条）→ 6.59 MiB（1.3.0 收尾：自定义分辨率与自定屏名）
→ 6.64 MiB（真机修复：会话判定、退出上限、窗口枚举提速、连点去重、搜索过滤）
→ 6.67 MiB（开机自启 `--minimized`、管道探测兜底、驱动自动安装的三处真机修复）
→ 5.55 MiB（本轮修复后改用 `opt-level = "s"`，尚未内嵌驱动）
→ 8.74 MiB（按单文件交付要求，内嵌原版驱动包、安装脚本与完整机型库）
→ 8.45 MiB（无损压缩字体、预生成托盘 RGBA）。

> 注意：`target/release/snug.exe` 正在运行时无法被覆盖，release 构建会报
> `failed to remove file`。要么先退出程序，要么用独立目录验证：
> `cargo build --release -p snug-app --offline --target-dir target\release-check`。

## 预算

**含驱动单 exe 上限 12 MiB（12,582,912 B）**：保留原应用部分 8 MiB 预算，
另给内嵌驱动 4 MiB 预算。原先的 8 MiB 仅对应驱动在 exe 外的交付形式。
发版脚本会打印尺寸；CI/本地发版前对一眼这个数。

## 已经做的

`Cargo.toml` 的 release profile：

```toml
opt-level = "s"     # 面向体积优化，保留循环向量化
lto = "fat"          # 全程序 LTO，跨 crate 内联 + 去重
codegen-units = 1    # 单 CGU，最大化死代码消除
strip = "symbols"    # 符号不进二进制
```

`panic = "abort"` 刻意没开：eframe/egui 有个别路径依赖 unwind。

内嵌驱动前，同一份代码只改优化等级，实测 `3` 为 6,999,552 B、`s` 为 5,817,856 B、
`z` 为 5,319,680 B。默认 `s` 较 `3` 缩小约 17%，字体、图标、功能和静态 CRT 均保留。
`z` 还会关闭循环向量化；本轮无窗口 egui 微基准约慢 7%，暂不作为默认。
微基准不能替代真实交互验收，测量方法和边界见
[`optimization-20261119.md`](optimization-20261119.md)。

依赖侧：

- `image` 只开 `png` + `ico` 两个 feature（栅格化 App 图标、托盘图标用）
- `windows` crate 按需开 feature，没有整包
- HTTP 走系统 `curl.exe` 子进程，不链 libcurl
- 截图 PNG 用自写的 stored deflate，不引压缩库
- COM/UIA/管道都是 `windows` crate 直接调，没有额外封装库

## 资源优化与测量边界

四份字体的完整字节与 SHA-256 固定；生成时用 Windows Compression API XPRESS_HUFF 无损压缩，
运行时校验原哈希后恢复到 epaint，原字形、face index、tweaks 和 fallback 顺序均保持。
压缩缓冲共 **860,970 B**，相对原始 **1,407,752 B** 少 **546,782 B**；本机单轮恢复约 4.2 ms。
字体许可证文本保留在 EXE About 页可查看。

本轮同一代码的 PE 结果：基线 9,377,280 B → `s` 8,862,720 B，减少 **514,560 B（5.49%）**。
减少量主要来自字体压缩和移除 ICO 解码路径；驱动 ZIP、机型库和业务功能仍保留。

托盘 ICO 已转为原 Rust `image 0.25.10` 解码与 Lanczos3 缩放结果的两份 32×32 RGBA。
生成脚本逐像素校验一致；RGBA 合计 8,192 B，原 ICO 合计 12,492 B，移除运行时解码。
app 不再直接依赖 `image`，但 eframe 的 PNG 剪贴板路径仍会经传递依赖使用 image。

`opt-level = "z"` 的同代码对照为 **8,341,504 B**，比 `s` 再少 521,216 B（5.88%）。
交替运行 6 轮的字体/中文/emoji egui 卡片布局与 tessellation 探针中，`s` 各轮中位数的中位数
为 260.14 μs/帧，`z` 为 279.75 μs/帧，约慢 7.5%。这个探针不是完整真机交互验收，
所以默认仍保留 `s`；`z` 只作为明确追求体积时的实验配置。

## 怎么量

```powershell
pwsh -File scripts\fetch-driver.ps1
cargo build --release -p snug-app --offline
(Get-Item target\release\snug.exe).Length
```

发版资产（含 zip 与校验和）：

```powershell
pwsh -File scripts\build-release.ps1
```

发版门禁（版本/体积/清单/依赖/发行包一次查完，不用启动程序）：

```powershell
pwsh -File scripts\preflight.ps1
```
