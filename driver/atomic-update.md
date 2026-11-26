# 驱动原子更新协议

应用与驱动源码在同一仓库维护。实现位于 `vendor/virtual-display-rs/driver-ipc/src/core.rs` 和 `virtual-display-driver/src/ipc.rs`。

## 请求与回复

沿用 V2 的 UTF-8 JSON 和 `0x04` 帧尾：

```json
{"SnugUpdate":{"monitors":[{"id":1,"name":"Snug","enabled":true,"modes":[{"width":1920,"height":1080,"refresh_rates":[60]}]}],"replacing":false}}
```

驱动回复以下一种结果：

```json
{"SnugUpdate":"Applied"}
{"SnugUpdate":"AlreadyExists"}
{"SnugUpdate":"NotFound"}
{"SnugUpdate":"InvalidData"}
{"SnugUpdate":"ApplyFailed"}
```

`replacing=false` 只创建新编号；任何目标编号已存在（包括禁用屏幕）时，整个批次返回 `AlreadyExists`。`replacing=true` 只替换已存在的编号；任意目标编号缺失时，整个批次返回 `NotFound`。重复编号、重复分辨率、重复刷新率、空模式或刷新率列表，以及零宽高或刷新率返回 `InvalidData`。数据与冲突检查均在同一个 `MONITOR_MODES` 锁内完成；这些检查拒绝时没有部分更新，未列出的屏幕始终保留。

驱动与旧版全量 `Notify` 共用数据验证：编号为 `0..=15`、最多 16 块屏，每块屏最多 256 种分辨率，每种分辨率最多 4 个刷新率，宽高为 `1..=16384`，刷新率为 `1..=240`，名称最多 256 个 UTF-8 字节且不含空字符。这与驱动声明的 16 个连接器及安全时序范围一致。

创建时，`Applied` 表示驱动已接受编号与模式意图。WDF 创建或到达失败时保留数据，删除尚未到达的 WDF 对象并清空对象缓存，使相同配置可以重试。应用必须先记录编号归属，再确认桌面实际上线；`Applied` 不等于显示器已上线。

`SnugUpdate` 仅重试本次请求的目标编号，不触发其他编号尚未完成的到达操作。旧版全量 `Notify` 仍可重试整张列表中尚未到达的屏幕。

替换只涉及已存在、归属已知的编号。WDF 撤离失败时保留原对象和原数据，不创建第二个对象；创建或到达失败时清空失败对象以便重试。这些物理操作失败返回 `ApplyFailed`，客户端可恢复原模式。一个替换批次内的物理操作可能部分成功，驱动不宣称它们具有物理原子性，应以随后的 `State` 和桌面实际状态为准。无论哪个撤屏入口，WDF 撤离失败的编号都会保留在 `State` 中，不得据此确认已删除。

`Notify`、`Remove`、`RemoveAll` 和 `SnugUpdate` 共同通过 `mutate_monitors` 持有 `MONITOR_MUTATIONS` 锁，覆盖状态更新、释放状态锁、WDF 到达操作及事件广播。只有实际缓存数据改变时，才向其他客户端广播与 `State` 一致的 `Changed`；部分失败时广播实际保留和更新后的数据，数据检查拒绝时不广播。这样也避免了在到达前删除编号或先后两次修改的事件逆序。只读 `State` 仍使用状态锁。

## 兼容边界

当前应用创建和维护的 V2 屏幕使用以下归属协议；`SnugUpdate` 保留为原有增量接口。

## 归属预留协议（0.4.0.5）

请求与回复继续使用相同 JSON/EOT 帧。`owner` 是每次创建生成的 32 位随机十六进制凭据，不得在撤销后重用。以下示例省略帧尾：

```json
{"SnugReserve":{"owner":"0123456789abcdef0123456789abcdef","id":1}}
{"SnugCreate":{"owner":"0123456789abcdef0123456789abcdef","monitors":[]}}
{"SnugReplace":{"owner":"0123456789abcdef0123456789abcdef","monitors":[]}}
{"SnugRelease":{"owner":"0123456789abcdef0123456789abcdef","ids":[1]}}
{"SnugRelease":{"owner":"0123456789abcdef0123456789abcdef","result":"Applied"}}
```

Create 和 Replace 的 `monitors` 必须是非空有效列表。回复使用与请求相同的键，值为 `{owner,result}`；客户端同时校验操作名和凭据。

Reserve 只预留编号，不创建屏幕。确认成功后，应用将意图写入独立的 `owned-displays.reservations.json`；落盘成功才允许 Create。原 `DisplayIds` 账本保留 C# 兼容格式。Reserve 超时或意图保存失败时不得发送 Create。若在预留后、落盘前崩溃，只会留下有租期的空预留。Legacy 不产生这份临时意图。Create、Replace、Release 的应答丢失或无法验证时归类为结果未知，禁止自动重放写请求；保留凭据以便清理。

驱动在同一修改锁中验证预留凭据、修改缓存及调用 WDF。空预留的租期为 30 秒，Reserve 与 Create 都清理过期空预留；已经接受的 Monitor 缓存保留凭据，不受租期影响，包括尚未到达 Windows 的屏幕。迟到 Reserve 最多暂占空编号一个租期。Release 撤销预留后，迟到 Create 因凭据不存在而失败。

Replace 必须持有原凭据。Release 只移除凭据匹配的编号，撤离失败返回 ApplyFailed 并保留凭据。成功移除本凭据的 Monitor 返回 Applied；该凭据已无 Monitor 或仅取消空预留返回 NotFound，应用可安全销账而不等待他人的同编号屏幕下线。驱动重启清除内存凭据，旧凭据不能删除重启后新建的资源。

`0.4.0.5` 至 `0.4.0.8` 曾允许旧版 Notify、Remove、RemoveAll、SnugUpdate 修改编号后使旧凭据失效。该历史行为可以绕过归属保护，已在 `0.4.0.9` 中移除，规则见下文。

## 无凭据写入保护（0.4.0.9）

四种无凭据写请求均在 `MONITOR_MUTATIONS` 内先检查 reservation，再执行缓存与 WDF 操作。检查、实际修改和事件广播之间没有异步等待，不能由另一个协议请求插入预留、撤销或接管。连接对应的 future 在回复阶段取消，不会中断已经执行的同步修改；带 owner 的请求仍按结果未知规则保留凭据，以便清理。

- `Notify` 是全量状态替换，也会重试尚未到达的显示。当前列表或请求列表只要包含已预留编号，整条命令直接拒绝；原样携带已拥有显示也不例外。
- `Remove` 的编号列表只要命中一个预留，整批拒绝，其他未拥有显示也不会被部分删除。
- `RemoveAll` 只要存在任意预留就拒绝，包括尚无 Monitor 的空预留。
- 无 owner 的 `SnugUpdate` 只要目标列表命中预留，就返回 `NotFound`，不创建或替换任何目标。未列出的已拥有编号继续保留。

拒绝时，Monitor 缓存、reservation owner 和租期、WDF 对象与原生调用计数均保持不变，驱动不广播 `Changed`。无凭据请求不撤销、消费、刷新或清理预留，即使空预留的租期已经到期。过期空预留仍仅由 `SnugReserve` 和 `SnugCreate` 清理，`SnugRelease` 可取消匹配凭据的空预留。

没有 owner 或 reservation 的旧客户端显示保持原有操作能力，包括 `Notify` 清空后恢复原 `State`。与 owner 资源共存时，无凭据增量更新和按编号删除仍可操作其他未拥有、未预留的编号；全量替换和全量删除按上述严格规则拒绝。`SnugCreate`、`SnugReplace` 和 `SnugRelease` 在同一修改锁内验证凭据后，才可调用内部应用函数。

## 归属查询与带归属模式调整（0.4.0.8）

```json
{"SnugCheck":{"owner":"0123456789abcdef0123456789abcdef","id":1}}
{"SnugCheck":{"owner":"0123456789abcdef0123456789abcdef","result":"Applied"}}
```

`SnugCheck` 独立持有 `MONITOR_MUTATIONS`，等待正在进行的显示修改完成，再读取已接受的 Monitor 缓存和预留凭据。有效编号、合法凭据、缓存存在且 owner 匹配时返回 `Applied`；编号不存在、仅有空预留或凭据不匹配时返回 `NotFound`；编号或凭据无效时返回 `InvalidData`。已接受但尚未到达 Windows 的缓存仍可通过归属检查；客户端还须独立确认桌面绑定。

该请求不走 `mutate_monitors`，不创建显示、不刷新或清理预留、不修改模式和 WDF 对象，也不广播 `Changed`。`SnugReserve` 继续只用于空编号预留，不能作为已有显示的归属探测。

`SnugCheck` 仅用于读回确认，不能授权随后按可复用 GDI 名或 `(adapter LUID, source_id)` 发起的原生写入。应用对 V2 屏幕只通过带 owner 的 `SnugCreate` / `SnugReplace` 发布当前目标的单一分辨率和刷新率；驱动在 `MONITOR_MUTATIONS` 内完成归属校验、缓存更新与 WDF 操作，拒绝已被其他 owner 接管的编号。应用不再对 V2 屏幕调用原生临时分辨率设置、持久 GDI 回退、DPI 设置或按 GDI 名附加桌面，因此不存在查询后再向复用原生标识写入的窗口。

驱动确认接受之后，应用继续确认 owner 与实际桌面绑定，并等待实测分辨率和刷新率匹配；未自动启用、读回失败、归属丢失或 Windows 未采用唯一目标模式时明确失败，保留清理所需凭据，不进行原生写入回退。`Applied` 仍不代表 Windows 已完成模式切换，真实系统自动收敛须单独进行硬件验收。

V2 保持 Windows 当前 DPI，窗口几何使用实际屏幕快照；用户可在 Windows 显示设置中手动调整 DPI。Legacy 为兼容旧驱动保留原生模式、DPI 和附加行为，不具备 V2 的 owner 原子保护。客户端严格匹配应答命令、owner 和 `Applied`；超时、未知应答、旧 helper 或旧 V2 驱动均停止，不自动重放或降级。无令牌绑定只允许 Legacy，归属查询本身不向 Legacy 管道写入命令。

## 旧协议与代理兼容

旧版 V2 驱动会忽略未知请求。单独调用 `SnugUpdate` 时必须取得明确回复；当前应用还要求上述 5 个归属请求获得确认。不支持时应返回错误，不得回退到读取快照再发送全量 `Notify`。旧版 Legacy 的 `Add` 路径保持独立。

应用与权限代理之间使用新的 `OwnedReserve`、`OwnedCreate`、`OwnedReplace`、`OwnedRelease` 和 `VerifyOwner` 请求名。已登记的旧版权限代理会拒绝这些未知请求名，无法将它们转成旧的驱动全量写入；升级应用后应重新登记新版权限代理。无令牌替换和删除分别使用 `LegacyOnlyReplace`、`LegacyOnlyRemove`；新 helper 在执行时再次确认 Legacy，旧 helper 拒绝未知名字。

Legacy 撤屏只能确认请求已提交。桌面下线后允许会话重新建屏，但保留尚未获得驱动确认的编号归属，新建时跳过这些编号。V2 确认编号已从驱动状态移除后，才释放其归属记录。

驱动对未拥有、未预留的显示保留旧版 `Notify` 全量替换语义。自 `0.4.0.9` 起，其他工具不能通过旧协议覆盖或删除 owner 保护的资源。增量协议继续保留未列出的显示，避免过时快照误删其他客户端刚创建的资源。
