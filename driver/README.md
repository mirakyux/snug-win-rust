# 虚拟显示器驱动

Snug 使用 [MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs) 创建 Windows 虚拟显示器。应用默认内嵌上游 [v0.3.1 安装包](https://github.com/MolotovCherry/virtual-display-rs/releases/tag/v0.3.1)，对应 [AGPLv3 源码](https://github.com/MolotovCherry/virtual-display-rs/tree/561fd024b3d00271f7d793e9d7f7390becc97352)。首次安装时会核对安装包和证书哈希。

## 源码与补丁

驱动源码位于 [vendor/virtual-display-rs](../vendor/virtual-display-rs/)。来源是上游 [22fcd2e](https://github.com/MolotovCherry/virtual-display-rs/tree/22fcd2e065aac13745cb27dc869848091b9e3a23)，采用 [MIT 许可证](../vendor/virtual-display-rs/LICENSE)。提交、归档哈希和驱动版本见 [source.lock.json](source.lock.json)。

仓库包含驱动、IPC、日志和 WDF 绑定。Snug 将其作为独立的 Cargo 工作区构建，使用上游固定的 Rust nightly。补丁保留 DXGI 设备的引用所有权，并在完成帧之前释放驱动持有的画面表面引用。构建脚本生成的版本为 `0.4.0.1`。

## 构建与签名

准备 PowerShell 7、Visual Studio C++ x64 工具链、Windows SDK 和 WDK，以及包含 `libclang.dll` 的 LLVM。已验证的 SDK 版本为 `10.0.26100.0`，驱动绑定使用 UMDF 2.31 与 IddCx 1.4。若使用独立的 WDK 或 LLVM 目录，可传入 `-WdkRoot` 和 `-LibClang`。

```powershell
pwsh -File scripts/build-driver.ps1 -WdkRoot 'C:\Path\to\WDK' -LibClang 'C:\Path\to\LLVM\bin'
```

脚本在 `target/driver-build/package/` 输出 DLL、INF、CAT 和许可证，并生成 ZIP 与 SHA-256 清单。它用 WDK 的 C++ 定义与 Rust 绑定对照 14 项布局，记录在 `target/driver-build/abi/verified.txt`。可以用 `-OutDir` 指定构建目录，用 `-Offline` 复用已缓存的 Rust 依赖。

使用已配置且受目标系统信任的代码签名证书时，传入本机证书存储中的 `-SignerThumbprint`。如果证书在计算机存储中，再加 `-MachineSigner`。脚本依次签名 DLL、生成 CAT、签名 CAT，并校验签名以及 CAT 中的 DLL 和 INF 哈希。公开分发需要符合目标系统的 PnP 驱动签名要求。见 [Windows 驱动签名类别](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/signature-categories-and-driver-installation)。

## 真机验收

独立 ZIP 交付驱动文件。当前 Snug 安装入口使用上游 MSI。要将新包接入发行版，需覆盖新装时创建 `Root\VirtualDisplayDriver` 设备、升级时复用已有设备，以及卸载时只清理 Snug 建立的设备。Windows 将驱动包加入驱动存储和创建根枚举设备视为两项操作。见 [微软驱动安装文档](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/using-the-devcon-tool-to-install-a-driver-package)。

用已签名的包在测试 Windows 上加载驱动。记录实际驱动版本、载入的 DLL 哈希和管道协议。用相同的配置与远控负载对比旧版和补丁版，覆盖竖屏、横屏、分辨率应用、反复建撤屏、睡眠唤醒和 UU 断开。每组记录撤屏后空闲时的资源变化，确认其他显示器保持原有状态。

当前版本已完成 x64 Release 编译、ABI 对拍及 Inf2Cat 检查。安装、长时资源测量和 UU 真机回归需使用签名后的产物。参见 [驱动验证记录](../docs/driver-hardening-20261123.md)。
