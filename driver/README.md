# 虚拟显示器驱动

Snug 使用 [MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) 创建 Windows 虚拟显示器。应用默认内嵌上游 [v0.3.1 安装包](https://github.com/MolotovCherry/virtual-display-rs/releases/tag/v0.3.1)，对应 [AGPLv3 源码](https://github.com/MolotovCherry/virtual-display-rs/tree/561fd024b3d00271f7d793e9d7f7390becc97352)。首次安装时会核对安装包和证书哈希。

## 源码与版本

驱动源码位于 [vendor/virtual-display-rs](../vendor/virtual-display-rs/)。来源是上游 [22fcd2e](https://github.com/MolotovCherry/virtual-display-rs/tree/22fcd2e065aac13745cb27dc869848091b9e3a23)，采用 [MIT 许可证](../vendor/virtual-display-rs/LICENSE)。提交、归档哈希和驱动版本见 [source.lock.json](source.lock.json)。

仓库直接维护驱动、IPC、日志和 WDF 绑定，并将它们作为独立的 Cargo 工作区构建，使用固定的 `nightly-2024-07-26`。当前源码候选版本为 `0.4.0.12`，日期为 `11/26/2026`；应用默认内嵌包仍为上游 `0.3.1`。

资源管理修复保留 DXGI 设备引用，在完成帧前释放画面表面，管理光标事件与上下文清理，并处理 MMCSS 注册失败时的 swapchain 释放。新增 `SnugUpdate` 原子请求在驱动锁内检查编号、合并并应用指定屏幕，保留其他屏幕；所有修改操作共同串行覆盖 WDF 到达阶段，避免另一客户端提前删除待到达屏幕。旧版 `Notify` 保留全量替换语义。请求、回复和兼容边界见 [原子更新协议](atomic-update.md)。

`0.4.0.4` 修复到达失败后的对象清理与重试、撤离失败时的对象和状态保留，以及竖屏和高分辨率的时序计算。替换物理失败返回 `ApplyFailed`；创建的 `Applied` 表示已接受编号，应用先记录归属再等待桌面上线。应用端对应修复见 [生命周期审计修复记录](../docs/lifecycle-audit-fixes-20261126.md)。

`0.4.0.5` 新增带随机归属凭据的 `SnugReserve`、`SnugCreate`、`SnugReplace`、`SnugRelease`。应用确认预留后保存创建意图，落盘成功才创建；应答丢失后用同一凭据清理，不以编号或相同配置推断归属。空预留 30 秒后失效，已接受创建的归属持续到撤除或外部替换。版本同时修复初始化失败传播与空模式缓冲查询。

`0.4.0.6` 修复交换链持续成功取帧时无法响应停止、清理线程一直等待的问题。每帧完成处理后检查退出信号，保留已有表面释放和交换链删除顺序。

`0.4.0.7` 限制单个管道请求的 JSON 负载为 512 KiB，结束符不计入大小，覆盖当前最大合法监视器配置。多个请求合并读取时逐帧处理，保留未完成的后续帧；超长请求断开该连接，扫描只覆盖新输入。日志初始化后台重试超过五分钟后停止，保留驱动可用，不再将 DriverEntry 的原始驱动对象当作 WDF 设备句柄报告失败；同轮日志成功优先于超时。

`0.4.0.8` 新增只读 `SnugCheck`，在驱动修改锁内确认已接受的显示仍归指定凭据所有。查询不刷新预留、不改变模式或拓扑，也不广播事件。当前应用用它核对 V2 模式调整后的归属，再结合实际桌面模式确认结果。查询不授权后续 Windows 原生写入；V2 模式修改由带 owner 的驱动请求完成，并保留 Windows 当前 DPI。

`0.4.0.9` 关闭无凭据旧命令绕过归属保护的入口。`Notify`、`Remove`、`RemoveAll` 和无 owner 的 `SnugUpdate` 在同一修改锁内先检查预留，再调用原生 API。请求触及已拥有或空预留编号时，整批拒绝；拒绝不修改缓存、凭据、租期和 WDF 对象，也不广播事件。全量 `Notify` 不能删除、替换或重试已拥有显示；无凭据 `SnugUpdate` 返回 `NotFound`。旧命令不再事后撤销凭据，也不主动清理过期空预留；过期清理仍由 `SnugReserve` 和 `SnugCreate` 执行。

`0.4.0.10` 和 `0.4.0.11` 是隔离实验候选，未作为生产源码交付。前者单独试验 `IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE`，后者仅为 `1280×720@60` 和 `1600×900@60` 生成首选详细时序描述符（DTD）。当前源码不包含该标志，也不采用两模式白名单。实验范围与结果见[权限与硬件验证记录](../docs/permissions-hardware-validation-20261126.md)。

`0.4.0.12` 根据首个请求模式及其首个刷新率生成 EDID 首选 DTD，并检查可表示范围和时钟量化误差。它保留厂商、产品和序列身份，仅调整首选 DTD、必要的最大像素时钟上限和校验和；无法表示时回退到同一序列号的原始 EDID。原有多模式、多刷新率 IPC 校验和模式回调保持不变。

未拥有、未预留的旧客户端显示继续支持全量更新、空状态清除与恢复、增量修改和撤屏。已有 owner 时，旧版增量请求仍可独立修改其他未拥有编号。V2 应用通过带 owner 的创建和模式替换发布唯一目标模式，不再依赖查询后按可复用原生标识写入。具体行为和兼容限制见 [原子更新协议](atomic-update.md)。

应用要求 V2 驱动支持这些带确认的归属请求。旧 V2 驱动不支持时返回错误，不回退到读取快照后发送全量 `Notify`。Legacy 的 `Add` 路径独立保留。应用与权限代理使用 `OwnedReserve`、`OwnedCreate`、`OwnedReplace`、`OwnedRelease` 和 `VerifyOwner` 请求名，升级后需重新登记新版权限代理；旧代理在写入驱动前拒绝未知请求。无令牌的旧会话仅能通过 Legacy 修改模式或撤屏，代理分别使用 `LegacyOnlyReplace` 和 `LegacyOnlyRemove`，防止旧代理绕过 V2 保护。Legacy 撤屏后仍待驱动确认的编号归属继续保留，新建跳过这些编号；V2 确认删除或凭据已不再拥有资源后才释放归属。

## 构建与签名

准备 PowerShell 7、Visual Studio C++ x64 工具链、Windows SDK 和 WDK，以及包含 `libclang.dll` 的 LLVM。已验证的 SDK 版本为 `10.0.26100.0`，驱动绑定使用 UMDF 2.31 与 IddCx 1.4。若使用独立的 WDK 或 LLVM 目录，可传入 `-WdkRoot` 和 `-LibClang`。

```powershell
pwsh -File scripts/build-driver.ps1 -WdkRoot 'C:\Path\to\WDK' -LibClang 'C:\Path\to\LLVM\bin'
```

脚本在 `target/driver-build/package/` 输出 DLL、INF、CAT 和许可证，并生成 ZIP 与 SHA-256 清单。它验证 DLL 文件和产品版本、INF 日期与版本均符合 `source.lock.json`，并用 WDK 的 C++ 定义与 Rust 绑定对照 14 项布局，记录在 `target/driver-build/abi/verified.txt`。可以用 `-OutDir` 指定构建目录，用 `-Offline` 复用已缓存的 Rust 依赖。

协议与资源回归分别运行：

```powershell
cargo +nightly-2024-07-26 test --offline --manifest-path vendor/virtual-display-rs/Cargo.toml -p driver-ipc --lib
pwsh -File driver/tests/run-atomic-update-tests.ps1 -Offline
pwsh -File driver/tests/run-resource-regressions.ps1
pwsh -File driver/tests/run-lifecycle-regressions.ps1
pwsh -File driver/tests/run-init-regressions.ps1
pwsh -File driver/tests/run-input-regressions.ps1
```

使用已配置且受目标系统信任的代码签名证书时，传入本机证书存储中的 `-SignerThumbprint`。如果证书在计算机存储中，再加 `-MachineSigner`。脚本依次签名 DLL、生成 CAT、签名 CAT，并校验签名以及 CAT 中的 DLL 和 INF 哈希。公开分发需要符合目标系统的 PnP 驱动签名要求。见 [Windows 驱动签名类别](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/signature-categories-and-driver-installation)。

## 真机验收

独立 ZIP 交付驱动文件。当前 Snug 安装入口使用上游 MSI。要将新包接入发行版，需覆盖新装时创建 `Root\VirtualDisplayDriver` 设备、升级时复用已有设备，以及卸载时只清理 Snug 建立的设备。Windows 将驱动包加入驱动存储和创建根枚举设备视为两项操作。见 [微软驱动安装文档](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/using-the-devcon-tool-to-install-a-driver-package)。

用已签名的包在测试 Windows 上加载驱动。记录实际驱动版本、载入的 DLL 哈希和管道协议。用相同的配置与远控负载对比旧版和补丁版，覆盖竖屏、横屏、分辨率应用、反复建撤屏、睡眠唤醒和 UU 断开。每组记录撤屏后空闲时的资源变化，确认其他显示器保持原有状态。

此前 `0.4.0.8` 的构建记录保留在 `target/owner-check-driver-build-20261126-final/`，离线签名副本保留在 `target/acceptance-complete-20261126/signed-candidate/`。这些旧产物不包含 `0.4.0.9` 的旧协议保护；`0.4.0.10` 和 `0.4.0.11` 仅用于隔离实验，均不能替代当前 `0.4.0.12` 源码候选。验收时应独立核对实际加载版本和签名后 DLL 哈希，不能仅凭目录名判断候选身份。

本轮生产生命周期回归在 Debug 和 Release 下各通过 45 项，覆盖无凭据混批拒绝、空预留保护、锁等待后的归属检查，以及未拥有显示的旧协议恢复。测试使用 WDF 替身，不等于真实驱动装载或显示行为验收。此前资源修复的验证记录见[驱动验证记录](../docs/driver-hardening-20261123.md)，此前持续取帧修复见[继续审查记录](../docs/continued-audit-fixes-20261126.md)，归属协议与交互修复见[多轮审查记录](../docs/overnight-audit-fixes-20261126.md)。
