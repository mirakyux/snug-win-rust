# UU 已连接但未建屏：Qt 控件适配

## 根因与修复

自动开屏及屏配置已启用，但旧解析器只接收 UIA Table / Row。
当前 UU Qt 客户端实际提供 `ControllerInfoView` 和
`ControllerInfoView.ControllerItemWidget` 分组，因此真实控制端一直被判为不可读，未进入建屏。

v1.0.2 在 `crates/snug-win/src/uia.rs` 将这两个精确的控件标识归一化为既有表结构：
必须保留真实的“本机被控中”直接标题，行仍须包含“远程控制”标记。
没有放宽普通分组，也未让 TCP 对端或推流增长独立开启会话。
`uu.rs` 的诊断文案同步明确区分可能的界面不匹配，不再只暗示读取超限。

新增 `crates/snug-win/examples/uu-controllers.rs` 用生产适配器进行只读探测，
只输出可读性、控制端数量和耗时，不输出设备名、密码或原始界面树。
`Cargo.toml` / `Cargo.lock` 更新本地版本到 1.0.2，`CHANGELOG.md` 记录修复。

## 验证

- 六项脱敏 Qt 树回归先红后绿；UIA 定向测试 18 项通过。
- 当前真实 UU 窗口的只读探测返回 `readable=true controller_count=1`，约 91 ms。
- Debug / Release 全仓测试各 575 项通过，1 项管理员 staging ACL 测试跳过。
- 两种 profile 的 Clippy correctness / suspicious 门禁通过，Release 构建无 rustc 警告。
- 打包、双实现 SHA-256 交叉校验和 18 项静态检查通过。
- 安装 EXE 与候选包哈希一致，配置保留，v1.0.1 EXE 与配置已备份。
- 13:57:42 运行版读到 1 个控制端并进入建屏；13:58:16 手机屏 #0 就绪。
- 本次真实连接建出 1080×1900 虚拟屏，显示器拓扑从 2 块增至 3 块，手势和界面接管成功。
- 随后的用户操作日志记录 QQ 窗口已铺入手机屏；本轮代理未发送任何 QQ 消息。

## 本机状态与验证边界

以下记录是 v1.0.2 的历史状态；当前安装已被 [v1.0.3 生命周期修复与验收](lifecycle-repair-20261122.md) 取代，残留屏已清理、底栏和断开撤屏已完成本机验收。

安装位置为 `%LOCALAPPDATA%\Programs\Snug\snug.exe`，本地版本 1.0.2。
备份位于该目录的 `backups\20261122-uu-qt-e6e14472\`。
EXE SHA-256：`4dd276b76193a305ccbeb70acfa09f99d3888776fcfce6fd3a5f66a3e93e3c77`。
未创建标签或发布 Release；源码同步与本地安装、正式发布分别管理。

后续发现终端启动遗留的 stderr 管道关闭导致一次打印异常，已改用 NUL 标准流的隐藏启动器启动同一份 EXE。
这次重启的驱动代理 UAC 返回用户取消，启动清理保留了 display #0 的归属记录。
当前等待用户在系统 UAC 中确认后重启并复核接管，不能将初次成功记录表述为最新进程已接管。
同时待确认约 14:01 的远端信号消失是否来自用户主动断开或退到后台。

UIA 仍依赖 UU 客户端提供被控端内容；本次没有用弱信号绕过此限制。
安装及运行验证文件位于 `target/uu-qt-repair-20261122/`。
