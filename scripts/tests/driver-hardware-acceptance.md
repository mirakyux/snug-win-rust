# Owner 协议驱动真机验收

本工具用于候选驱动的真实管道及桌面验收。默认 `Prepare` 仅构建独立测试程序、运行纯逻辑测试和显示帮助；这些结果不能作为硬件通过证据。工具不会安装驱动、请求提权、启动 Snug 或发送按键。显式 Smoke 会发送旧协议写入请求，验证候选拒绝绕过 owner 保护；只允许在原驱动 State 为空时执行，清理始终使用带 owner 的 Release。

## 1. 远控风险与装载前准备

驱动更新、设备重启、卸载或系统重启都可能撤离已有虚拟屏，并清空驱动内存中的 owner。若 UU 等远控依赖该屏，当前连接可能中断。即使 smoke 只增删自己的测试屏，Windows 的拓扑重算仍可能影响远控。应先具备本地控制台或经过验证的独立恢复路径，再由操作者统一执行系统操作。

更新前记录实际 `Root\VirtualDisplayDriver` 实例 ID、当前 OEM INF、版本、活跃桌面、观察到的 WUDFHost 加载模块路径及其磁盘文件 SHA256。保留已有驱动的完整可信安装包，并使用 `pnputil /export-driver <实际 OEM INF> <独立备份目录>` 导出当前包。磁盘文件哈希不等于进程内存哈希；记录必须说明证据口径。

仓库的 `driver/Install-SnugDriver.ps1` 安装上游 0.3.1 MSI，不能用它加载当前本地 `0.4.0.12` 源码候选。`0.4.0.10` 和 `0.4.0.11` 是隔离实验版本，不作为当前生产交付。候选 ZIP 的签名状态取决于构建时是否指定签名证书；加载前须独立核验签名、证书信任及系统策略，不应通过关闭签名强制来规避装载要求。

对于已经存在的根设备，受控的本地候选装载通常包括 `pnputil /add-driver <可信候选 INF> /install`，然后根据实际枚举结果决定是否重启该实例。导入 DriverStore 不等于已加载目标 DLL，也不保证创建缺失的 `Root\VirtualDisplayDriver` 根设备。设备不存在时，应先使用可信安装器或已核验的根枚举安装工具；本脚本不创建设备。

回滚应使用导出的原包及原实例记录：先重新安装可信原 INF，重新枚举并核验实际版本/模块文件；若 Windows 排名仍选中候选，只有在独立控制路径下才处理明确记录的候选 OEM INF，再安装原包。不要批量删除显示驱动，不要将其他虚拟显示设备误认成目标设备。原虚拟屏和 owner 状态不会因 INF 回滚自动恢复，需要单独验收原业务恢复。

## 2. 准备测试程序

在仓库根目录执行：

```powershell
./scripts/tests/run-driver-hardware-acceptance.ps1 -Mode Prepare
```

默认输出位于 `target/driver-hardware-acceptance/`。构建只使用本地 `snug-win`、`snug-core` 和仓库原有的 serde_json 依赖，采用 offline 模式；不更改生产 Cargo.toml。`prepared.json` 记录程序及参与准备的源码哈希。源码在构建期间或准备后发生变化时，Preflight/Smoke 会拒绝运行，需要重新 Prepare。Cleanup 保留使用先前已校验程序的能力。

本地纯逻辑测试覆盖：避开应用的 0..3 连接器；同时检查 State 和桌面中的空闲 ID；枚举顺序不敏感；原屏变化或无关新增屏失败；必须收到可信的错误 owner 拒绝；仅驱动 State 一致不能冒充实际模式收敛；恢复 owner 格式校验。

## 3. 只读预检

从已经以管理员身份运行的终端执行：

```powershell
./scripts/tests/run-driver-hardware-acceptance.ps1 -Mode Preflight
```

该步骤记录设备与加载模块文件，查询 V2 State 和随机 owner 的只读 SnugCheck，并比较前后桌面。旧协议、未支持 SnugCheck、无唯一主屏、重复虚拟 ID 或不稳定桌面均失败。它不预留连接器、不创建屏幕、不启动权限 broker，也不构成完整硬件验收通过。

## 4. 显式执行真实 smoke

仅在独立控制路径和签名候选装载均已核验后执行。`ExpectedDriverSha256` 必须来自独立核验的已安装候选 DLL；若做了 Authenticode 签名，应使用签名后的文件哈希，而非原未签名 ZIP 内 DLL 哈希。

```powershell
./scripts/tests/run-driver-hardware-acceptance.ps1 `
  -Mode Smoke -Execute `
  -ExpectedDriverVersion '0.4.0.12' `
  -ExpectedDriverSha256 '<签名后候选 DLL 的实际 64 位十六进制 SHA256>'
```

运行前必须恰有一台健康的目标根设备，版本匹配，且所有已观察到的 VirtualDisplayDriver.dll 加载模块文件均匹配指定哈希。默认使用新的 run 目录；可用 `-RunDirectory` 指定不存在的新目录。

测试过程：

1. 只选 State 和活跃桌面都未使用的单个高位 ID（4..15），不会占用应用常用的 0..3。
2. 将随机 owner、错误 owner、ID、初始 State 和所有活跃屏幕快照写入 `owner-journal.json`，并同步文件后才发送 Reserve。
3. Reserve 必须确认，且不得改变拓扑。空预留的 SnugCheck 必须返回 NotFound；它只检查已接受的显示，不检查空预留。Create 后再要求 SnugCheck 确认所有权，并等待 1280×720@60 在驱动 State 和真实桌面连续两次同时匹配。
4. 在仅有本轮测试屏的条件下，依次发送旧 Notify（替换、遗漏、相同配置）、Remove、RemoveAll 及无 owner 的 SnugUpdate。每次通过同一连接上的 State 回复确认请求已处理，检查原模式、凭据与所有原屏不变；SnugUpdate 还必须明确返回 NotFound。原 State 非空时提前拒绝，不发送 Reserve。
5. 错误 owner 的 Check、Replace、Release 必须被明确拒绝；原 owner 和 1280×720 模式必须保持。不能把超时、结果未知或其他错误当成拒绝成功。
6. 正确 owner Replace 只发布 1600×900@60，等待驱动及真实桌面连续两次收敛。
7. 始终按同一 owner Release，确认归属已失效、测试屏在 State/桌面均消失。每次快照都核对原有显示器、模式、刷新率、DPI、位置、主屏和工作区未改变。

不会用 GDI、SetDisplayConfig、DPI 写入或按键推动收敛。发现无关屏变化后停止后续测试，只尝试 owner 范围内清理，不按可能过期的快照强制恢复其他屏。未知的 Create/Replace 结果不会自动重放；Release 是单 owner 的幂等恢复操作。

每次模式等待最多 15 秒；整个 smoke 子进程最多 180 秒。正常失败会清理，wrapper 还会在非零退出或超时且 journal 存在时运行一次独立 Cleanup。cleanup 子进程最多 60 秒。系统卡死、断电、整个终端被杀或进程创建失败时仍可能需要人工按 journal 恢复。

## 5. 独立清理与证据标准

保留失败 run 目录；不要手工换 token 或删除 journal。若需要恢复：

```powershell
./scripts/tests/run-driver-hardware-acceptance.ps1 `
  -Mode Cleanup -RunDirectory '<原 run 目录的实际绝对路径>'
```

也可直接运行准备后的独立程序（路径有空格时使用 PowerShell 调用运算符 `&`）：

```powershell
& './target/driver-hardware-acceptance/cargo/debug/snug-driver-hardware-acceptance.exe' `
  cleanup '<原 run 目录的实际绝对路径>'
```

Cleanup 只读取原 journal 的 owner/ID，不会 RemoveAll，也不会清理其他 owner 的屏。如果 owner 已被外部修改而同 ID 被他人复用，原 owner 的 Release 无权删除它；工具将保留失败证据，不扩大清理范围。

完整通过必须同时保留：

- Prepare 的构建/单测记录和 `prepared.json`；
- `inventory-before.json` / `inventory-after.json`，包含实际 PnP 版本、OEM INF、加载模块文件路径/哈希及签名状态；
- 原始 `owner-journal.json`、逐阶段 `events.jsonl`、stdout/stderr；
- `result.json` 中 `passed: true` 且 `cleanupPassed: true`，包括成功的新建、两种模式实际收敛、错误 owner 拒绝及最终释放；
- 原有屏幕在所有观察点均未改变，并且操作后实际远控链路仍可用的独立观察。

这是一轮有界 smoke。它不能替代长时资源、睡眠唤醒、重启、并发多客户端、真实 UU 操作及复杂显卡组合测试，也不能证明读取快照之间没有短暂显示扰动。
