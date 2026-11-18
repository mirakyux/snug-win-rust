# 体积预算

单文件、零运行时依赖是本项目的一条硬要求（C# 原版要装 .NET 运行时）。
原版发版包也就几 MB，我们**不能**变成几十 MB 的那种"Rust 大块头"。

## 当前实测

| 项 | 大小 | 时间 |
| --- | --- | --- |
| `target/release-check/release/snug.exe`（1.3.0） | 6,899,200 B（6.58 MiB） | 2026-11-19 |
| `dist/Snug-v1.3.0-win-x64-selfcontained.zip` | 3,202,919 B（3.05 MiB） | 2026-11-19 |

历史：5.09 MiB → 6.13 MiB（加托盘、截图、UU 探测、App 资源库）
→ 6.26 MiB（修 bug）→ 6.56 MiB（多屏 `DeskSet`、主题、控制中心设置页、重入胶囊）
→ 6.59 MiB（退出手机屏确认条）→ 6.58 MiB（1.3.0 收尾，版本资源变化）。

> 注意：`target/release/snug.exe` 正在运行时无法被覆盖，release 构建会报
> `failed to remove file`。要么先退出程序，要么用独立目录验证：
> `cargo build --release -p snug-app --offline --target-dir target\release-check`。

## 预算

**上限 8 MiB（8,388,608 B）**。超过就先去减，而不是继续加。
发版脚本会打印尺寸；CI/本地发版前对一眼这个数。

## 已经做的

`Cargo.toml` 的 release profile：

```toml
opt-level = 3
lto = "fat"          # 全程序 LTO，跨 crate 内联 + 去重
codegen-units = 1    # 单 CGU，最大化死代码消除
strip = "symbols"    # 符号不进二进制
```

`panic = "abort"` 刻意没开：eframe/egui 有个别路径依赖 unwind。

依赖侧：

- `image` 只开 `png` + `ico` 两个 feature（栅格化 App 图标、托盘图标用）
- `windows` crate 按需开 feature，没有整包
- HTTP 走系统 `curl.exe` 子进程，不链 libcurl
- 截图 PNG 用自写的 stored deflate，不引压缩库
- COM/UIA/管道都是 `windows` crate 直接调，没有额外封装库

## 还能省的地方（按收益排序）

1. ~~关掉 egui 自带字体~~ **已评估，不做**。`default_fonts` 打包的 `Hack`/`Ubuntu-Light`
   之外还有 **`NotoEmoji` 与 `emoji-icon-font`**——界面上的 `⏏` `📱` `‹` `×` 全靠它们
   渲染（见 `ui.rs`/`tray.rs`）。关掉会全变豆腐块；自己内嵌一份等大的字体并不会更小，
   等于把省下的几百 KB 原样塞回去。要省只能改成纯 ASCII 图标，那是界面改版，不是体积优化。
2. `windows` crate 的 feature 按符号裁剪时再看一遍，有大块没用到就关掉。
3. 图标资源（`snugwin-idle.ico` / `snugwin-connected.ico` / logo）内嵌，
   多尺寸 ICO 只留 16/20/24/32/48 就够。

## 怎么量

```powershell
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
