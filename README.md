# Snug for Windows

Snug 是配合网易 UU 远程使用的 Windows 工具。它在电脑上创建虚拟手机屏，把选中的窗口放到屏上，方便在手机上操作。

这是独立维护的 Rust 重写版，交付内嵌虚拟显示器驱动的单个 EXE。 [更新记录](CHANGELOG.md)

## macOS Snug 功能对齐

| macOS Snug 版本 | Windows 开发版功能 |
| --- | --- |
| [v2.9.0](https://github.com/zephyr4123/snug/releases/tag/v2.9.0) | 电脑虚拟屏的左缘 App 资源库，支持按窗口位置分组、搜索、切换和移动窗口。 |
| [v2.8.2](https://github.com/zephyr4123/snug/releases/tag/v2.8.2) | 跟进 UU 切屏防误断。接入 Windows 捕获监视后，同一会话短暂重建抓屏时会保留连接状态。 |
| [v2.8.0](https://github.com/zephyr4123/snug/releases/tag/v2.8.0) | CrossDesk 连接检测和绝对鼠标手势仍处于实验阶段，默认关闭。 |

## 下载

从 [GitHub Releases](https://github.com/mirakyux/snug-win-rust/releases/latest) 下载 `snug.exe`。
把文件放在当前用户可写的目录，双击即可启动。程序内嵌虚拟显示器驱动安装包和安装脚本。

Snug 默认以普通权限运行，启动后会在后台准备驱动连接。已用管理员身份启动时，Snug 直接连接驱动。普通权限启动时，如果驱动限制访问，Snug 会在启动阶段通过 UAC 请求授权。首次安装虚拟显示器驱动也需要管理员权限。

也可以在仓库目录运行 `pwsh -File scripts/install-windows.ps1`。脚本从 GitHub Releases 下载 `snug.exe`，核对 SHA-256 后放到 `%LOCALAPPDATA%\Programs\Snug`。覆盖同一路径的旧版本前，请先从托盘退出 Snug。

### CrossDesk（实验性）

在「桌面设置 → 通用 → 偏好」开启「启用 CrossDesk（实验性）」。如果 CrossDesk 未发现虚拟屏，在「屏幕 → 手机屏显示」选「始终显示」，等虚拟屏就绪后重启 CrossDesk，再从控制端选屏。手机端可选绝对鼠标以尝试触控。

会话检测适用于同一 Windows 登录会话中的 CrossDesk 桌面程序。手机连入、常驻主机识屏、切屏和断开仍待真机验证。

## 虚拟显示器驱动

Snug 用 virtual-display-rs 创建和撤销手机屏，由 Windows 设置实际显示模式，UU 负责采集和传输画面。Snug 为手机屏配置固定分辨率和 60 Hz，应用切换时保持当前分辨率。

在「通用 → 驱动与日志」可查看系统安装的驱动版本、设备问题代码和管道访问状态。如果驱动连接中断或取消了管理员授权，可在实体屏上点「重试连接」重新授权。分辨率应用失败时，屏幕详情页允许重新应用同一尺寸。

如需登录 Windows 后无人值守，在「桌面设置 → 通用 → 偏好」勾选「登录后后台启动」。如果驱动需要管理员权限，首次设置时用当前登录账户完成一次 UAC 确认。此后登录同一账户，Snug 会在后台启动并连接驱动，登录过程无需确认。升级前已开启登录自启的用户，退出托盘中的 Snug 再手动打开，即可完成首次授权。取消勾选会停止登录自启，已授权的驱动仍可在手动打开 Snug 时使用。

仓库包含固定提交的驱动源码和两处画面资源释放补丁，可独立构建 Windows x64 驱动 `0.4.0.1`。在驱动发行签名和真机验收完成前，Snug 的常规构建和发布继续内嵌上游 `v0.3.1` 安装包。详见 [驱动构建与验收](driver/README.md)。

## 更新

在设置的「通用 → 更新与关于」中点击「检查更新」，或开启「启动时自动检查新版本」。程序默认查询本仓库的 GitHub Releases。
找到更高版本后，可以从程序内下载并重启更新。自动安装需要普通权限且 EXE 所在目录可写。

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
  phone-models.json   手机、平板和电脑机型库
  *.ico               应用与托盘图标
docs/                 移植对照、验收单、多屏与线程模型、体积预算
driver/               驱动安装、源码版本记录和独立构建说明
vendor/virtual-display-rs/  固定提交的驱动源码与资源释放补丁
references/           上游源码的浅克隆，仅供对照，已在 .gitignore 中
```

## 功能

手机屏资源库包含「运行中」「全部」和搜索。运行中的 App 卡片直接列出窗口，全部应用采用四列网格，横屏显示 App 和窗口双栏。设置包含「屏幕」「交互」和「通用」，更新、驱动和日志位于「通用」。

双击托盘图标或选择「打开桌面设置」，可在实体屏上打开带侧栏和紧凑表单的桌面设置。桌面与手机面板共用配置，分辨率在点击「应用分辨率」后同步生效。

在「屏幕 → 手机屏显示」或托盘的「手机屏显示」菜单中，可选择「关闭」「连接时显示」或「始终显示」。「连接时显示」会在远控连入后建屏，会话结束后撤屏。「始终显示」在 Snug 运行时保持已启用的虚拟屏可用，退出 Snug 时撤屏。显示方式会保存，下次启动继续使用。

在桌面设置的「屏幕」页或托盘菜单打开「虚拟屏投影」，即可查看当前在线虚拟屏。单击画面会在虚拟屏的对应位置执行一次左键点击。托盘菜单也可直达「驱动与日志」。

在「屏幕」中添加并启用「电脑」类型的虚拟屏，然后从屏幕菜单切换过去。点击电脑虚拟屏左缘的「应用资源库」，可切换到这块屏上的窗口、移入其他位置的窗口或打开应用。搜索框支持应用名、窗口标题和拼音，按 Enter 执行第一项。移入的窗口可从资源库还原。屏幕名称可在桌面设置的屏幕详情中修改。

当 UU 缩到托盘时，Snug 可根据服务端持续的硬件视频编码判断有远控连入。接入可选的 UU 捕获监视后，同一会话短暂重建抓屏时会保留连接状态，减少切屏误断。打开已运行的单实例应用时，Snug 也会尝试接续它唤醒的已有窗口。 [捕获监视与真机验收](docs/acceptance.md)

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

**开机自启**在设置的「通用」页开启（「登录后后台启动」）。它注册一条计划任务
（`SnugWinAutoStart`，登录后延迟 20 秒、普通权限、参数 `--minimized`）：启动后后台常驻托盘，
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

若需要从源码编译驱动，请准备 WDK 与 LLVM，再执行下面的命令。脚本输出 DLL、INF、CAT、校验和与 ABI 对照结果。参数与签名方法见 [驱动构建指南](driver/README.md)。

```powershell
pwsh -File scripts/build-driver.ps1
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
- [docs/driver-hardening-20261123.md](docs/driver-hardening-20261123.md) — 驱动可靠性改进与验证记录
- [docs/ui-parity.md](docs/ui-parity.md) — 界面与原版对照
- [docs/ui-fusion-20261122.md](docs/ui-fusion-20261122.md) — 界面融合与验证记录
- [docs/size-budget.md](docs/size-budget.md) — 体积预算与已做的取舍
- [docs/quality.md](docs/quality.md) — 质量门禁（rustc 零警告、clippy 怎么用）

## 许可

Snug 应用和仓库中的驱动源码使用 MIT 许可证。默认内嵌的旧版驱动安装包使用 AGPLv3，附有对应版本的源码和许可证链接。详见 [应用许可证](LICENSE) 与 [第三方声明](THIRD_PARTY_NOTICES.md)。
