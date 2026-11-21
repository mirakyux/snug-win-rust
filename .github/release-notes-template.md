## Snug v{{VERSION}}

Snug 是配合网易 UU 远程使用的 Windows 工具。它在电脑上创建虚拟手机屏，把选中的窗口放到屏上，方便在手机上操作。
这个独立维护的 Rust 版本从 `v1.0.0` 开始发布。

### 下载与运行

下载 `snug.exe`，将它放在当前用户可写的目录，双击运行。程序内嵌虚拟显示器驱动和安装脚本，无需单独安装 .NET 或 Visual C++ 运行库。

| 资产 | 说明 |
| --- | --- |
| `snug.exe` | 可直接运行的单文件 |
| `Snug-v{{VERSION}}-win-x64-selfcontained.zip` | 应用内更新包，包含同一个 `snug.exe` |
| `checksums.txt` | EXE 和 ZIP 的 SHA-256 校验和 |

主界面以普通权限启动。首次安装驱动时，Windows 会单独请求管理员权限。已有驱动但普通权限无法访问时，
首次建屏也可能请求管理员权限，以启动受限的驱动代理。需要修复驱动时，打开设置的「驱动与日志」页。

### 更新

打开设置的「关于与更新」页，点击「检查更新」。Snug 默认从本仓库的 GitHub Releases 检查正式版本。
需要定期检查时，可开启「启动时自动检查新版本」。应用内安装更新需要使用普通权限，且 EXE 所在目录可写。

如果还在使用沿用上游编号的本地开发版 `1.3.0`，请退出旧进程，再手动替换为本仓库的发布版 EXE，以接入独立发布序列。

### 本版变化

{{COMMITS}}

完整差异：{{COMPARE}}

### 已知限制

- 实机验证以 Windows 11 x64 和网易 UU 远程为主。Windows 10 2004 及更高版本尚未完成验证。
- 最多配置 4 块屏（手机、平板或电脑），同一时刻只创建选中的一块。
- 换机型比例后，在 UU 远程中重新选择屏幕，让点按位置与画面匹配。
- Rust 版与 C# 版共用默认数据目录和开机启动任务。请退出其中一个版本后再运行另一个。

### 致谢与许可

- 上游 macOS 原版：[zephyr4123/snug](https://github.com/zephyr4123/snug)
- 上游 Windows C# 版：[nicocart/snug-win](https://github.com/nicocart/snug-win)
- 虚拟显示器驱动：[MolotovCherry/virtual-display-rs](https://github.com/MolotovCherry/virtual-display-rs)
- 本项目采用 MIT 许可，详见 [第三方声明](https://github.com/mirakyux/snug-win-rust/blob/{{TAG}}/THIRD_PARTY_NOTICES.md)。
