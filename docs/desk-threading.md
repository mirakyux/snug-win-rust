# 桌面的线程与锁

`DeskSet` / `Desk` / `SerialLane` 这套东西有四条线程，搞错顺序就是卡死或"该撤的屏不撤"。
这一页是砌逐行复查后确认下来的约束，改 `controller.rs` 或 `desk/` 之前先看。

## 四条线程

| 线程 | 干什么 |
| --- | --- |
| **界面线程**（主线程） | egui 每帧：读状态快照、画界面、把 desk 操作塞进 `pending` 队列 |
| **app worker**（`snug-desk-idle`，`controller.rs`） | 每 500ms 一拍：取 `pending` 执行、采样 UU、算屏愿望、发布快照 |
| **lane 线程**（`snug-desk`，`serial_lane`） | **真正执行 desk 动作**（建屏/撤屏/铺窗/还原），按入队顺序 |
| **watch / retry 线程** | 窗口出现/退出/关闭的等待；建屏失败后的重试阶梯（5/15/30s） |

关键一条：**动作体永远在 lane 线程上跑**。`DeskSet::idle()` 只是"投一条空动作然后等它跑完"，
它不会让动作跑到调用线程上来。

## 三条硬规矩

### 1. 界面线程绝不在 `DeskSet` 的锁上等

`desks: Arc<Mutex<DeskSet>>` 的锁由 app worker 持有。界面线程只能：

- **读**：`Controller::state_of()` 读 `snapshots`（worker 发布的拷贝），不碰锁；
- **写**：`Controller::enqueue()` 往 `pending` 队列里塞闭包，由 worker 代执行。

违反这条的后果是真机上出现过的那种：建屏期间界面整块卡死几秒。

### 2. 不要在每拍里调 `DeskSet::idle()`

`idle()` 的语义不是"排空队列"，而是"**等到所有 desk 都 quiet**"——
`pending_retry` 为空且 `watches` 为空。watch 最长：打开窗口 30s、退出 10s、
退出卡死后还要跟 60s；而且它等 watch 是忙等（一次 channel 往返一轮，不 sleep），会吃满一个核。

在每拍里调它的代价（都是真的会发生）：

- 这期间 `set_wishes` 进不去 → **会话结束时撤屏被推迟到 watch 结束** →
  "退出远控后虚拟屏还挂着"会原样复现；
- 这期间 `publish_snapshots` 也进不去 → 界面拿不到新状态；
  比如"退出卡住 / 强制退出"那张卡片要等 watch 结束（最多 ~70s）才出现，
  而那正是用户该点它的时候；
- 加屏失败重试阶梯（5/15/30s）期间，`set_wishes`、拓扑轮询、关闭窗口全部停摆。

重试和 watch 都是独立线程、自己会推进，`check_returns()` 只是入队——
**一拍都不用等它们**。当前实现里 worker 只做 `check_returns()` + 发布快照。
`idle()` 留给测试（等静止）用。

### 3. 回调在 lane 线程上

`log` / 单 desk 的 `state_changed` / `topology_changing` / set 级的 `state_changed`
都是在 **lane 线程**里被调的。所以：

- `on_change` 只许 `request_repaint()`，不许碰 `desks` 锁（worker 持锁 → lane 要锁 = 死锁）；
- 现在的 `topology_changing` 回调只锁 `session`（`RemoteSession`），安全；
- 日志 sink 里也不要去读 desk 状态。

## 关闭顺序

`Controller::shutdown()` 是**先 join worker、再 `desks.shutdown()`**，不能反：

- 先 shutdown 再 join，worker 可能还跑一拍 `set_wishes` → `set_wanted(true)` → 刚撤的屏又建回来；
- 反过来不会死等：`DeskSet::shutdown` 阻塞的是 lane 线程，lane 由 `DeskSet` 自己攥着，
  join 之后照样能跑完。

代价：lane 是串行的，关闭要排在正在飞的动作后面。最坏 = 一个 create（几秒）
\+ `MINIMUM_LIFETIME 2.5s`（屏刚建不到 2.5 秒时）+ `TOPOLOGY_SETTLE 1.0s` + 窗口还原。
体感上"退出会卡一下"，屏刚建好那一下最明显。

## 屏的选取：`None` 到底指哪块

`DeskSet::desk_for_screen(None)` 的规则是"先找第一块触屏，没有再取第一块"（C# 同）。
但 app 侧另外两处必须和它对齐，否则会出现"建的是电脑副屏、界面却在找手机屏"：

- `Controller::state_of(None)` → `config.primary_touch_screen()`（优先竖屏手机）
- `active_wishes(..., None)` → **也**用 `primary_touch_screen()` 的 id

`state_of(Some(id))` 查不到时返回 `DeskState::default()`，**不要**退到"随便拿一块"：
`HashMap` 迭代顺序随机，多块 desk 时会静默显示错的那块。

## 已知的、照跟 C# 的问题

- `Preparing → Ready` 在同一个 lane 动作里完成，界面看不到"准备中"这个中间态
  （C# 也是这样，只是它的 UI 有别的提示）。
- `wait_for_online` 的"任意新屏"兜底在多屏同时上线时可能认错屏（`C4`）。
- `set_mode` 按 `stable_id == display_id` 匹配（`C4` 老风险）。
- 两版同时开满 4 块虚拟屏时，候选 id 轮转会回到 0 撞号（单版本 ≤4 无问题）。
