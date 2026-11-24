# Snug for Windows

Snug 是配合网易 UU 远程使用的 Windows 工具。它在电脑上创建虚拟手机屏，把选中的窗口放到屏上，方便在手机上操作。

这是独立维护的 Rust 重写版，从 **v1.0.0** 开始发布，交付含驱动的单个 EXE。它源自 macOS 的
[Snug](https://github.com/zephyr4123/snug) 和 Windows 的 C# 移植版
[SnugWin](https://github.com/nicocart/snug-win)。虚拟显示器由
[MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) 提供。版本变更见 [更新记录](CHANGELOG.md)。

## 下载

从 [GitHub Releases](https://github.com/mirakyux/snug-win-rust/releases/latest) 下载 `snug.exe`。
把文件放在当前用户可写的目录，双击即可启动。程序内嵌虚拟显示器驱动安装包和安装脚本。

主界面以普通权限运行。缺少虚拟显示器驱动时，首次安装会单独请求管理员权限。已安装的驱动若只允许管理员访问，首次建屏时程序会请求提升驱动代理的权限。

## 虚拟显示器驱动

Snug 用 virtual-display-rs 创建和撤销手机屏，由 Windows 设置实际显示模式，UU 负责采集和传输画面。Snug 为手机屏配置固定分辨率和 60 Hz，应用切换时保持当前分辨率。

在“通用 → 驱动与日志”可查看系统安装的驱动版本、设备问题代码和管道访问状态。如果驱动连接中断或取消了管理员授权，可以点“重试驱动连接”。分辨率应用失败时，屏幕详情页允许重新应用同一尺寸。

Snug 内嵌上游 `v0.3.1` 安装包。修复了两处画面资源释放问题的 `0.4.0.1` 驱动在独立分支开发，接入发行版前需完成发行签名和真机验收。见 [驱动开发分支](https://github.com/mirakyux/snug-win-rust/tree/feature/driver-0.4.0.1)。

## 更新

在设置的「通用 → 更新与关于」中点击「检查更新」，或开启「启动时自动检查新版本」。程序默认查询本仓库的 GitHub Releases。
找到更高版本后，可以从程序内下载并重启更新。自动安装需要普通权限且 EXE 所在目录可写。

如果曾运行本地开发版 `1.3.0`，请先退出旧进程，再用发布版 `v1.0.0` 的 `snug.exe` 替换原文件。
自动更新只识别更高的版本号，无法用旧开发版切换到独立发布序列。

C# 版与 Rust 版各有自己的更新渠道。两版共用默认数据目录和开机启动任务，请退出其中一个版本后再运行另一个。

## 为什么重写

- `.NET` 桌面 UI（WPF）不支持 NativeAOT，自包含发布要 180 MB 磁盘；Rust 版目标是 10 MB 级别。
  实际交付为**内含驱动的单 exe**，大小与取舍见 [体积预算](docs/size-budget.md)。
- 原版最值钱的是行为细节（窗口归位、字号记忆、手势判定的各种边界），
  这些都在 `SnugWin.Core` 里，可以逐文件对照移植，并有单测兜底。
- 平台层用 `windows` crate 直接调 Win32 / COM，省掉一层托管封装。

## 目录

```text
crates/
  snug-core/   纯逻辑层，零 Win32 依赖（geometry / screen_fit / touch / scrolling /
               phone_models / config / record / left_behind / crop / zoom / store /
               screen_plan / desk 状态机）
  snug-win/    Windows 平台层（虚拟屏 IPC + CCD、DWM 搬窗、底层鼠标钩子、UIA、
               shell 图标、截图、UU 连接探测、自动更新）
  snug-app/    托盘程序与手机屏界面（egui/eframe）
assets/
  phone-models.json   机型库（853 款：手机 / 平板 / 电脑），与 C# 版共用
  *.ico               应用与托盘图标
docs/                 移植对照、验收单、多屏与线程模型、体积预算
driver/               虚拟显示器驱动安装脚本
references/           上游源码的浅克隆，仅供对照，已在 .gitignore 中
tools/refdump/        C# 参考实现的对拍生成器（几何 / ScreenFit 真值 TSV）
```

## 功能

v1.0.5 的手机界面按原版的运行中、全部和搜索页面调整：
顶部使用返回当前 App 的胶囊和屏幕菜单，运行中的 App 卡片直接列出窗口，全部应用采用四列网格，横屏使用 App / 窗口双栏。
设置精简为「屏幕 / 交互 / 通用」，更新、驱动和日志收在通用页。融合范围与验收要求见 [界面融合记录](docs/ui-fusion-20261122.md)。

桌面可运行 `snug.exe --preview` 查看界面。预览中的资源库动作不会启动、关闭或搬移应用；设置仍可修改真实配置或执行系统操作。
实际手机屏按物理短边统一缩放。桌面预览用于检查布局，真实远控操作仍需验收。

可直接指定预览页面，并保存当前程序视口的 PNG 截图：

```powershell
snug.exe --preview-all
snug.exe --preview-search=edge
snug.exe --preview-settings=screens    # 也可选 interaction、general 或 diagnostics
snug.exe --preview --preview-output=preview.png
```

`--preview-output` 使用 egui 的帧缓冲截图，约 3 秒后写入指定路径；可与上述页面参数组合使用。输出目录需事先存在。

屏幕直接按分辨率配置，不必选择机型。新屏默认 1080×1920，提供常见竖屏、横屏、平板预设和自定义宽高；点击“应用分辨率”才生效，应用切换不会改变分辨率。旧配置保留原有像素尺寸。

| 模块 | C# 源 | 状态 |
|---|---|---|
| 几何 / 屏幕适配 | `Geometry` `ScreenFit` | ✅ 已移植 + 单测 + C# 真值对拍 |
| 手势状态机 | `TouchTranslator` `Scrolling` | ✅ 已移植 + 单测 |
| 机型库 | `PhoneModels` | ✅ 已移植 + 单测（含真实 assets 校验） |
| 配置 / 路径 | `AppConfig` `SnugPaths` | ✅ 已移植 + 单测（1.2 配置自动迁移到多屏结构） |
| 记录 / 存储 | `PhoneRecord` `JsonFileStore` `LeftBehind` | ✅ 已移植 + 单测（按屏分文件） |
| 裁剪 / 字号 | `CropGeometry` `ZoomBook` | ✅ 已移植 + 单测 |
| 窗口会话状态机 | `PhoneDesk` | ✅ 已移植 + 单测（共享串行队列、多屏 `DeskSet`） |
| 拼音搜索 | `PinyinSearch` | ✅ 已移植 + 单测 |
| 应用枚举 / 会话判定 | `Apps` `Session/*` `ControllerTable` | ✅ 已移植 + 单测 |
| 平台层 | `SnugWin.Windows` | ✅ 已移植 + 单测（虚拟屏 IPC / CCD / DWM / 钩子 / UIA / 截图 / 更新） |
| 界面 | `PhoneHomeWindow` 等 WPF | ✅ egui 重写（触控资源库、三类设置、主题、重入胶囊、桌面预览） |

全仓单测覆盖核心状态机、Win32 适配器、应用与更新事务（数量以当前测试摘要为准），
`rustc` 零警告是硬门禁，见 [docs/quality.md](docs/quality.md)。

**开机自启**在设置的「通用」页开启（「登录后后台启动」）。它注册一条计划任务
（`SnugWinAutoStart`，登录触发、普通权限、参数 `--minimized`）：开机后后台常驻托盘，
**不弹窗、不抢焦点**，手机屏就绪了才把界面摆上虚拟屏。换 exe 目录后会自动把任务里的
路径改过去。

自动更新仅在普通权限下执行，使用 EXE 同级的独占暂存目录。受保护安装目录请手动替换 EXE。需要管理提升权限的应用窗口时，可在设置里显式选择“以管理员重启”，更新前应退出并以普通权限启动。

与原版的差别与取舍理由集中在 [`docs/upstream-watch.md`](docs/upstream-watch.md)；
多屏形态（单窗口 + 屏幕菜单切换）见 [`docs/multi-screen.md`](docs/multi-screen.md)；
线程与锁纪律见 [`docs/desk-threading.md`](docs/desk-threading.md)。
本轮响应优化、编译体积对照与验证边界见 [`docs/optimization-20261119.md`](docs/optimization-20261119.md)。

## 构建

需要 Rust（stable 1.89+，edition 2024）+ MSVC 工具链 + Windows SDK。

```powershell
pwsh -File scripts/fetch-driver.ps1    # 编译前准备内嵌驱动，核对 SHA-256
cargo test --workspace
cargo build --release -p snug-app
pwsh -File scripts/build-release.ps1    # dist/snug.exe；另保留自动更新 zip 与校验和
```

`target\release\snug.exe` 正在运行时无法被覆盖，验证构建可用独立目录：

```powershell
cargo build --release -p snug-app --offline --target-dir target\release-check
```

## 发版

在 GitHub Actions 中运行 `Release`。工作流依据 `Cargo.toml` 的版本构建和打包，通过检查后创建 Release 草稿。
验收同一份 EXE 后，运行 `Publish verified draft` 校验并公开草稿。发布页提供单个含驱动的 EXE，以及应用内更新所需的 ZIP 和校验和。

本地等价流程与资产命名规则见 [docs/release.md](docs/release.md)。

## 文档

- [docs/acceptance.md](docs/acceptance.md) — 真机与更新验收
- [docs/release.md](docs/release.md) — 发版流程（版本号、门禁、资产名、更新源）
- [docs/real-machine-test.md](docs/real-machine-test.md) — 真机测试记录
- [docs/display-parity.md](docs/display-parity.md) — 虚拟屏链路与原版逐行对拍
- [docs/ui-parity.md](docs/ui-parity.md) — 界面与原版对照
- [docs/ui-fusion-20261122.md](docs/ui-fusion-20261122.md) — v1.0.5 界面融合与验证记录
- [docs/size-budget.md](docs/size-budget.md) — 体积预算与已做的取舍
- [docs/quality.md](docs/quality.md) — 质量门禁（rustc 零警告、clippy 怎么用）

## 许可

Snug 应用使用 MIT 许可证。内嵌的上游 `v0.3.1` 驱动安装包使用 AGPLv3，附有对应版本的源码和许可证链接。详见 [应用许可证](LICENSE) 与 [第三方声明](THIRD_PARTY_NOTICES.md)。
