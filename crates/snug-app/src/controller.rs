//! 应用控制器：把 `snug-core` 的手机屏状态机接到 `snug-win` 的平台实现上。
//!
//! 对应 C# 版 `SnugWin.App.AppController`。区别在于本移植不引 async 运行时：
//! `snug-core::desk::Desk` 的方法只入队，真正的动作在它自己的工作线程里串行执行（对应 C# `SerialLane`）。
//! 控制线程处理界面动作和屏幕愿望；UU 探测在独立线程运行，只发布最新结果。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use snug_core::desk::{
    AppControl, DeskSet, DeskSetEvents, DeskSetOptions, DeskState, PhoneDisplays, PhoneScreenPhase,
};
use snug_core::session::{ControllerTable, UUSignals};
use snug_core::touch::{TouchOptions, TouchRegion};
use snug_core::windows::{AppInfo, WindowInfo};
use snug_core::{AppConfig, Paths, Rect};
use snug_win::{WinPhoneDisplays, WinWindows};

/// `snug-win` 的 `WinApps` 已经实现了 core 的 [`AppControl`]
/// （`apps.rs` 的 `impl<S: AppSystem> AppControl for Apps<S>`），控制器直接用真机实现。
use snug_win::WinApps;

/// 手势钩子抛给**界面**的事件（对应 C# `TouchHookService` 的选区两个事件）。
///
/// 关闭窗口那一路不在这里：它由后台线程直接 `desk.close_placed(handle)` 处理，
/// 不经过界面（界面关掉也没关系）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchEvent {
    /// 选区结束（物理像素坐标）→ 界面在选区上方弹"复制"气泡。
    SelectionCompleted(snug_core::Point),
    /// 选区被取消 → 收起气泡。
    SelectionDismissed,
}

/// 钩子线程把事件塞进队列，应用侧每帧取走（对应 C# 里 `Dispatcher.BeginInvoke`）。
#[derive(Default)]
struct TouchEventQueue {
    close: std::sync::Mutex<Vec<u64>>,
    selection: std::sync::Mutex<Vec<TouchEvent>>,
}

impl TouchEventQueue {
    fn push_close(&self, handle: u64) {
        self.close.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }

    fn push(&self, event: TouchEvent) {
        self.selection.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }

    fn take_close(&self) -> Vec<u64> {
        std::mem::take(&mut *self.close.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn take_selection(&self) -> Vec<TouchEvent> {
        std::mem::take(&mut *self.selection.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// 给 `TouchHook` 用的事件接收端。
struct HookEvents {
    queue: Arc<TouchEventQueue>,
}

impl snug_win::touch::TouchEvents for HookEvents {
    fn close_window_requested(&self, handle: u64) {
        self.queue.push_close(handle);
    }

    fn selection_completed(&self, at: snug_core::Point) {
        self.queue.push(TouchEvent::SelectionCompleted(at));
    }

    fn selection_dismissed(&self) {
        self.queue.push(TouchEvent::SelectionDismissed);
    }
}

/// 界面线程排给后台线程的 desk 操作。
///
/// 界面线程不直接 `lock()` 那个 `DeskSet`（后台可能正在里面建屏），
/// 而是把要做的动作塞进队列，由后台线程在两次 `idle()` 之间执行。
type DeskAction = Box<dyn FnOnce(&mut DeskSet) + Send>;

const UU_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const UU_SAMPLE_MAX_AGE: f64 = 5.0;

struct UuSample {
    sampled_at: f64,
    signals: snug_win::UuSignalSet,
}

type LatestUuSample = Arc<std::sync::Mutex<Option<UuSample>>>;

enum UuReading {
    Fresh(snug_win::UuSignalSet),
    Pending,
    Stale,
}

fn read_uu_sample(
    latest: &std::sync::Mutex<Option<UuSample>>,
    now: f64,
    last_sample_at: &mut f64,
) -> UuReading {
    let sample = latest.lock().unwrap_or_else(|e| e.into_inner()).take();
    match sample {
        Some(sample) if now - sample.sampled_at <= UU_SAMPLE_MAX_AGE => {
            *last_sample_at = sample.sampled_at;
            UuReading::Fresh(sample.signals)
        }
        _ if now - *last_sample_at > UU_SAMPLE_MAX_AGE => UuReading::Stale,
        _ => UuReading::Pending,
    }
}

/// 控制器：持有状态机与后台线程，界面只读它。
pub struct Controller {
    /// 多屏管理器（对应 C# `DeskSet`）：一块屏一个 desk，共用一条串行队列。
    ///
    /// `DeskSet` 的写操作（`set_wishes`/`set_remote`/`shutdown`）要 `&mut self`，
    /// 所以外面套了 `Mutex`。
    ///
    /// **锁的纪律**（很重要，别破坏）：
    /// - 后台线程在 `idle()` 里会**执行队列里的慢操作**（建屏最长 8 秒 × 4 个候选），
    ///   这段期间锁是被它占着的；
    /// - 所以界面线程**绝不能**在锁上等：读走 [`Self::snapshots`]（后台每拍刷新），
    ///   写走 [`Self::pending`] 队列由后台线程代劳。
    ///   之前直接在界面线程 `lock()` 过一次，结果是建屏期间界面整块卡死。
    desks: Arc<std::sync::Mutex<snug_core::desk::DeskSet>>,
    /// 后台线程每拍刷新的状态快照（屏 id → 状态），界面读它就不必碰 `desks` 的锁。
    snapshots: Arc<std::sync::Mutex<std::collections::HashMap<String, DeskState>>>,
    /// 界面线程排给后台线程的 desk 操作（`pull`/`open`/`set_wishes` …）。
    pending: Arc<std::sync::Mutex<Vec<DeskAction>>>,
    /// 最近一次屏配置（wishes 由它算，写屏列表时更新）。
    config: Arc<std::sync::Mutex<AppConfig>>,
    /// 界面当前显示哪块屏（`None` = 主触控屏）。
    ///
    /// 多屏时**只有这块屏会真的建出来**：我们只有一个窗口，建成 N 块虚拟屏的话
    /// 其余几块是纯黑屏，比"看不到"更糟。所以同一时刻只保留一块，
    /// 切换就是换一台设备（旧屏撤、新屏建）。见 `docs/multi-screen.md`。
    active_screen: Arc<std::sync::Mutex<Option<String>>>,
    /// 手机屏（界面窗口要搬到它的位置上，所以留一份）。
    displays: Arc<WinPhoneDisplays>,
    /// 手势钩子（C# `TouchHookService`）。
    touch: Arc<snug_win::TouchHook>,
    /// 用户明确要求保留手机屏，不依赖 UU 探测是否有结果。
    manual_override: Arc<AtomicBool>,
    /// 手机屏总开关（对应 C# `_config.PhoneScreen`）。
    ///
    /// 它只是"允许开屏"，真正开不开还要看会话：C# 每个 tick 都算
    /// `_config.PhoneScreen && _session.InEpisode && WantsPhoneScreen(...)` 再 `SetWanted`。
    /// 移植时漏了这一层（只在启动时设一次），真机表现就是"退出远控后虚拟屏还挂着"。
    phone_enabled: Arc<AtomicBool>,
    /// UU 远程会话状态机（控制线程消费采样结果，界面读它判断"在不在会话里"）。
    session: Arc<std::sync::Mutex<snug_core::session::RemoteSession>>,
    /// 上一次真的装到钩子上的区域；`set_region` 会起线程，不能每帧重复装。
    touch_region: std::sync::Mutex<Option<TouchRegion>>,
    /// 钩子抛出的事件队列（关闭窗口 / 选区结束 / 选区取消）。
    touch_events: Arc<TouchEventQueue>,
    /// "选后复制"真正按快捷键的那一层（C# `WindowService.PerformSelectionActionAsync`）。
    selection_actions: Arc<snug_win::WinSelectionActions>,
    #[allow(dead_code)] // 后续设置界面/诊断要用
    paths: Paths,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Controller {
    /// 按配置建起状态机与平台实现，并启动后台线程。
    ///
    /// `on_change` 在状态机每次推送新状态时被调用（界面据此重绘）。
    pub fn start(
        config: &AppConfig,
        paths: &Paths,
        on_change: impl Fn() + Send + Sync + 'static,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let displays = Arc::new(WinPhoneDisplays::with_paths(paths));
        let windows = Arc::new(WinWindows::new());
        let apps: Arc<dyn AppControl> = Arc::new(WinApps::new());

        // 会话状态机与它用的时钟：`DeskSetEvents::topology_changing` 回调也要用到
        // （C# `_desks.TopologyChanging += () => _session.NoteSelfInducedChange(now)`），
        // 所以两边必须是同一个 `RemoteSession`、同一根时间轴。
        let clock = std::time::Instant::now();
        let session = Arc::new(std::sync::Mutex::new(snug_core::session::RemoteSession::new()));

        let events = DeskSetEvents {
            state_changed: Some(Arc::new(move || on_change())),
            gone: None,
            topology_changing: Some(Arc::new({
                let session = session.clone();
                move || {
                    if let Ok(mut session) = session.lock() {
                        session.note_self_induced_change(clock.elapsed().as_secs_f64());
                    }
                }
            })),
            log: Some(Arc::new(log)),
        };
        let options = DeskSetOptions {
            timeline: None,
            default_zoom: config.zoom_steps,
            events,
        };

        let desks = Arc::new(std::sync::Mutex::new(DeskSet::new(
            displays.clone(),
            windows,
            apps,
            paths,
            options,
        )));
        let snapshots: Arc<std::sync::Mutex<std::collections::HashMap<String, DeskState>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let pending: Arc<std::sync::Mutex<Vec<DeskAction>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        // 清理可能等待系统 UAC：放到控制线程，在第一条 wish 前完成，
        // 不让它挡住主线程上的界面与托盘创建。
        let startup_displays = displays.clone();
        // 注意：这里**不**按 `config.phone_screen` 直接开屏。C# 的 `SetWishes` 由
        // `OnTick` 每秒按会话算一次（见下面 worker），启动时没有任何 wish、
        // 屏幕要等第一段远程会话才建。移植时漏了这一层，真机表现是"退出远控后
        // 虚拟屏还挂着，只有杀进程才消失"。

        let running = Arc::new(AtomicBool::new(true));
        let stop = running.clone();
        let desks_worker = desks.clone();
        let manual_override = Arc::new(AtomicBool::new(false));
        let manual_worker = manual_override.clone();
        let latest_uu: LatestUuSample = Arc::new(std::sync::Mutex::new(None));
        {
            let latest = latest_uu.clone();
            let stop = running.clone();
            // UIA 调用可能被外部进程卡住：控制线程读最新结果，退出时也不等待探测线程。
            std::thread::Builder::new()
                .name("snug-uu-sampler".to_string())
                .spawn(move || {
                    let sensor = snug_win::UuSensor::new();
                    while stop.load(Ordering::Relaxed) {
                        let began = std::time::Instant::now();
                        let sampled_at = began.duration_since(clock).as_secs_f64();
                        let signals = sensor.sample(sampled_at);
                        let elapsed = began.elapsed();
                        if !stop.load(Ordering::Relaxed) {
                            break;
                        }
                        *latest.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(UuSample { sampled_at, signals });
                        if elapsed > UU_SAMPLE_INTERVAL {
                            crate::warn!("UU：一次探测耗时 {} ms", elapsed.as_millis());
                        }
                        std::thread::sleep(UU_SAMPLE_INTERVAL.saturating_sub(elapsed));
                    }
                })
                .map_err(|error| {
                    std::io::Error::new(error.kind(), format!("无法启动 UU 探测线程：{error}"))
                })?;
        }
        let phone_enabled = Arc::new(AtomicBool::new(config.phone_screen));
        let phone_enabled_worker = phone_enabled.clone();
        let session_worker = session.clone();
        let config = Arc::new(std::sync::Mutex::new(config.clone()));
        let config_worker = config.clone();
        let active_screen: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let active_screen_worker = active_screen.clone();
        let snapshots_worker = snapshots.clone();
        let pending_worker = pending.clone();
        // 钩子要在起后台线程之前建好：线程要 drain 它抛出的"关闭窗口"事件。
        let touch_events = Arc::new(TouchEventQueue::default());
        let touch = Arc::new(snug_win::TouchHook::with_parts(
            Arc::new(snug_win::touch::WinInput),
            Arc::new(HookEvents { queue: touch_events.clone() }),
        ));
        let touch_events_worker = touch_events.clone();
        let worker = match std::thread::Builder::new()
            .name("snug-desk-idle".to_string())
            .spawn(move || {
                startup_displays.cleanup_orphans();
                if let Ok(desks) = desks_worker.lock() {
                    desks.recover();
                }
                // 显示设置变化：C# 用 `SystemEvents.DisplaySettingsChanged`，
                // 我们轮询显示器拓扑签名（更省事，也不需要消息窗）。
                use snug_win::display::DisplayApi;
                let display = snug_win::WinDisplay::new();
                let mut last_topology: Option<Vec<(u32, i32, i32, i32, i32)>> = None;
                // 与 `topology_changing` 回调共用同一根时间轴（会话状态机内部按秒比较）。
                let started = clock;
                let mut last_policy_update = -1.0_f64;
                let mut last_uu_sample = 0.0_f64;
                let mut uu_sample_stale = false;
                let mut last_manual = false;
                let mut last_topology_check = -1.0_f64;
                let mut last_check_returns = -1.0_f64;
                let mut last_connected = false;
                let mut last_signals = None;
                while stop.load(Ordering::Relaxed) {
                    // 界面排过来的动作先做（只是入队，很快）：这样界面线程永远不用在
                    // `DeskSet` 的锁上等——那把锁在 `idle()` 里可能被建屏占住好几秒。
                    let actions: Vec<DeskAction> = pending_worker
                        .lock()
                        .map(|mut queue| std::mem::take(&mut *queue))
                        .unwrap_or_default();
                    if !actions.is_empty()
                        && let Ok(mut desks) = desks_worker.lock()
                    {
                        for action in actions {
                            action(&mut desks);
                        }
                    }
                    // 状态快照：发一份给界面（后台线程是唯一的发布者）。
                    publish_snapshots(&desks_worker, &snapshots_worker);
                    // **不要在这里调 `DeskSet::idle()`**（砌复查的结论）：
                    // 它不是"排空队列"，而是"等到所有 desk 都 quiet"——
                    // 有 watch 在跑时要等到 watch 结束（打开窗口 30s、退出 10s、
                    // 卡死跟随再 60s），而且期间是忙等、吃满一个核。
                    // 更糟的是这期间 `set_wishes` 进不去，会话结束时撤屏会被推迟，
                    // "退出远控后虚拟屏还挂着"那个 bug 会原样复现。
                    // 重试与 watch 都是独立线程、自己会推进；`check_returns` 只是入队。
                    // `check_returns` 在有 left-behind 条目时要枚举窗口、读标题，
                    // 单次真机实测 **3 秒**。原先每拍（500ms）入队 → 生产 2 个/秒、
                    // 消费 0.33 个/秒 → 共享队列无限积压（`[lane] 动作排队 219.8s`），
                    // 用户点 App 要排几分钟才执行。现在 5 秒一次：被接管的窗口晚几秒
                    // 回来用户无感，队列也不会被堵。
                    let now = started.elapsed().as_secs_f64();
                    if now - last_check_returns >= 5.0 {
                        last_check_returns = now;
                        if let Ok(desks) = desks_worker.lock() {
                            desks.check_returns();
                        }
                    }
                    publish_snapshots(&desks_worker, &snapshots_worker);
                    // 用户点了窗口自带的关闭按钮（C# `_desks.ClosePlaced(handle)`）。
                    for handle in touch_events_worker.take_close() {
                        crate::info!("手势：点了窗口关闭按钮，关掉 {handle:#x}");
                        if let Ok(desks) = desks_worker.lock() {
                            desks.close_placed(handle);
                        }
                    }
                    let now = started.elapsed().as_secs_f64();
                    let manual = manual_worker.load(Ordering::Relaxed);
                    if now - last_policy_update >= 1.0 || manual != last_manual {
                        last_policy_update = now;
                        last_manual = manual;
                        let signals = if manual {
                            // 手动强制只影响 wish；不要把合成信号写进真实远程会话状态。
                            None
                        } else {
                            match read_uu_sample(&latest_uu, now, &mut last_uu_sample) {
                                UuReading::Fresh(sampled) => {
                                    if uu_sample_stale {
                                        crate::info!("UU：探测已恢复");
                                        uu_sample_stale = false;
                                    }
                                    let summary = (
                                        sampled.client_running,
                                        sampled.client_visible,
                                        sampled.capture_active,
                                        sampled.peer_active,
                                        sampled.controllers.names.len(),
                                        sampled.controllers.readable,
                                    );
                                    if last_signals != Some(summary) {
                                        crate::info!(
                                            "UU 探测：客户端={} 服务端可见窗候选={} 推流={:?} 对端={} 控制端={}（{}）",
                                            summary.0,
                                            summary.1,
                                            summary.2,
                                            summary.3,
                                            summary.4,
                                            if summary.5 { "可读" } else { "读不到" }
                                        );
                                        last_signals = Some(summary);
                                    }
                                    Some(sampled.to_signals())
                                }
                                UuReading::Pending => None,
                                UuReading::Stale => {
                                    if !uu_sample_stale {
                                        crate::warn!(
                                            "UU：超过 {UU_SAMPLE_MAX_AGE} 秒没有新鲜探测结果，按无新证据推进会话"
                                        );
                                        uu_sample_stale = true;
                                    }
                                    Some(UUSignals {
                                        overlay_visible: false,
                                        controllers: ControllerTable::unreadable(),
                                        capture_active: None,
                                    })
                                }
                            }
                        };
                        // 会话判定用 `RemoteSession`（不只是 ConnectionTracker）：C# 的
                        // `_session.InEpisode` 决定"该不该有屏"，中断后还有一段重连窗口，
                        // 单用 `ConnectionTracker` 会把这段丢掉。
                        let (connected, in_episode, controllers_len, ended) = {
                            let mut session =
                                session_worker.lock().unwrap_or_else(|e| e.into_inner());
                            let ended = signals.is_some_and(|signals| {
                                session.update(&signals, now).iter().any(|t| {
                                    matches!(t, snug_core::session::SessionTransition::Ended)
                                })
                            });
                            (
                                session.is_connected(),
                                session.in_episode(),
                                session.controllers().len(),
                                ended,
                            )
                        };
                        if connected != last_connected {
                            last_connected = connected;
                            crate::info!(
                                "远程会话：{}（控制端 {} 个）",
                                if connected { "已连入" } else { "已断开" },
                                controllers_len
                            );
                        }
                        if ended {
                            crate::info!("远程会话：本次会话结束，撤掉手机屏");
                        }
                        // C# `OnTick`：
                        //   bool remote = _session.InEpisode || _sensor.ManualOverride;
                        //   _desks.SetWishes(ScreenPlan.Wishes(_config, remote));
                        //   _desks.SetRemote(_session.InEpisode);
                        // 与 1.2 的区别：不再按控制端类型判断要不要开屏（多设备时代那套
                        // 启发式已经不准了），只看"有没有会话/手动强制"，屏列表自己带 enabled。
                        let remote = in_episode || manual;
                        let enabled = phone_enabled_worker.load(Ordering::Relaxed);
                        let active = active_screen_worker
                            .lock()
                            .ok()
                            .and_then(|guard| guard.clone());
                        let wishes = {
                            let config =
                                config_worker.lock().unwrap_or_else(|e| e.into_inner());
                            active_wishes(&config, enabled, remote, active.as_deref())
                        };
                        if let Ok(mut desks) = desks_worker.lock() {
                            desks.set_wishes(wishes);
                            // 手动强制影响屏愿望，但保留原语义：DeskSet 的"远控"标记只来自真实会话。
                            desks.set_remote(in_episode);
                        }
                    }
                    if now - last_topology_check >= 2.0 {
                        last_topology_check = now;
                        // 删除请求失败时拓扑不会变化，善后仍需定时推进。
                        if let Ok(desks) = desks_worker.lock() {
                            desks.settle_displays();
                        }
                        let topology: Vec<(u32, i32, i32, i32, i32)> = display
                            .active_monitors()
                            .iter()
                            .map(|monitor| {
                                (
                                    monitor.stable_id,
                                    monitor.bounds.x as i32,
                                    monitor.bounds.y as i32,
                                    monitor.bounds.width as i32,
                                    monitor.bounds.height as i32,
                                )
                            })
                            .collect();
                        if let Some(previous) = &last_topology
                            && *previous != topology
                        {
                            crate::info!(
                                "显示器拓扑变化（{} → {} 块屏），通知状态机重新核对",
                                previous.len(),
                                topology.len()
                            );
                            if let Ok(desks) = desks_worker.lock() {
                                desks.displays_changed();
                            }
                        }
                        last_topology = Some(topology);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
        {
            Ok(worker) => worker,
            Err(error) => {
                // The sampler may be blocked inside an external UIA provider; signal it
                // to stop, but do not block startup cleanup waiting for an uncancellable COM call.
                running.store(false, Ordering::Relaxed);
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("无法启动手机屏控制线程：{error}"),
                ));
            }
        };


        Ok(Self {
            desks,
            snapshots,
            pending,
            config,
            active_screen,
            displays,
            touch,
            manual_override,
            phone_enabled,
            session,
            touch_region: std::sync::Mutex::new(None),
            touch_events,
            selection_actions: Arc::new(snug_win::WinSelectionActions::new()),
            paths: paths.clone(),
            running,
            worker: Some(worker),
        })
    }

    /// 主触控屏的 id（单屏界面默认操作的那块）。
    fn primary_screen_id(&self) -> Option<String> {
        let config = self.config.lock().ok()?;
        config.primary_touch_screen().map(|screen| screen.id)
    }

    /// 所有已启用触控屏的 `(id, 名字)`，按配置顺序（多屏切换用）。
    pub fn touch_screens(&self) -> Vec<(String, String)> {
        let Ok(config) = self.config.lock() else {
            return Vec::new();
        };
        config
            .effective_screens()
            .into_iter()
            .filter(|screen| screen.enabled && screen.device.kind.is_touch())
            .map(|screen| (screen.id, screen.name))
            .collect()
    }

    /// 指定屏的状态快照；`None` 表示"主触控屏"。
    ///
    /// **只读后台线程发布的快照**，绝不在 `DeskSet` 的锁上等：那把锁属于后台线程，
    /// 而它可能在等一条慢动作（建屏/等窗口），界面每帧都要读状态（见字段上的锁纪律）。
    ///
    /// 指定了 id 却查不到时返回默认值，**不要**退到"随便拿一块屏"：
    /// `HashMap` 的迭代顺序是随机的，多块 desk 时会静默显示错的那块。
    pub fn state_of(&self, screen_id: Option<&str>) -> DeskState {
        let id = screen_id.map(str::to_string).or_else(|| self.primary_screen_id());
        let Ok(snap) = self.snapshots.lock() else {
            return DeskState::default();
        };
        match id {
            Some(id) => snap.get(&id).cloned().unwrap_or_default(),
            // 一张屏配置都没有（异常配置）：只能拿任意一块，总比空白好。
            None => snap.values().next().cloned().unwrap_or_default(),
        }
    }

    /// 当前要展示的那块屏的状态快照（主触控屏）。
    pub fn state(&self) -> DeskState {
        self.state_of(None)
    }

    /// 屏列表变了（设置里增删改屏，对应 C# `_desks.SetWishes(ScreenPlan.Wishes(...))`）。
    ///
    /// 配置当场存下，`set_wishes` 交给后台线程执行（界面线程不在那把锁上等）。
    pub fn apply_screens(&self, config: &AppConfig) {
        if let Ok(mut stored) = self.config.lock() {
            *stored = config.clone();
        }
        let enabled = self.phone_enabled.load(Ordering::Relaxed);
        let remote = self.in_session() || self.manual_override();
        let active = self.active_screen.lock().ok().and_then(|guard| guard.clone());
        let wishes = active_wishes(config, enabled, remote, active.as_deref());
        let zoom = config.zoom_steps;
        crate::info!(
            "屏配置变化：{} 块屏愿望（remote={remote} 总开关={enabled}）",
            wishes.len()
        );
        self.enqueue(move |desks| {
            desks.set_wishes(wishes);
            desks.set_default_zoom(zoom);
        });
    }

    /// 切换界面当前显示哪块屏（`None` = 主触控屏）。
    ///
    /// 只保留这块屏的愿望：旧屏会被撤掉、新屏建出来，等于换一台设备。
    pub fn set_active_screen(&self, screen_id: Option<String>) {
        if let Ok(mut active) = self.active_screen.lock() {
            if *active == screen_id {
                return;
            }
            *active = screen_id.clone();
        }
        crate::info!(
            "切换屏幕：{}",
            screen_id.clone().unwrap_or_else(|| "主触控屏".to_string())
        );
        let enabled = self.phone_enabled.load(Ordering::Relaxed);
        let remote = self.in_session() || self.manual_override();
        if let Ok(config) = self.config.lock() {
            let wishes = active_wishes(&config, enabled, remote, screen_id.as_deref());
            self.enqueue(move |desks| desks.set_wishes(wishes));
        }
    }

    /// 重新进入手机屏：撤销用户按下的"收起"（C# `ReenterAllDismissedTouch`）。
    pub fn reenter(&self) {
        if !self.phone_enabled.load(Ordering::Relaxed) {
            crate::warn!("重新进入手机屏：总开关是关的，忽略");
            return;
        }
        crate::info!("重新进入手机屏");
        self.enqueue(|desks| desks.reenter_screen(None));
    }

    /// 当前这一段远程会话还在不在（对应 C# `_session.InEpisode`）。
    pub fn in_session(&self) -> bool {
        self.session.lock().map(|session| session.in_episode()).unwrap_or(false)
    }

    /// 能不能"重新进入手机屏"（C# `CanReenterPhoneScreen`）：
    /// 会话中（或手动强制）**且**有触控屏被用户收起过。
    ///
    /// 读快照（`dismissed` 是屏状态的一部分），不碰 `DeskSet` 的锁。
    pub fn can_reenter(&self) -> bool {
        let dismissed = self
            .snapshots
            .lock()
            .map(|snap| snap.values().any(|state| state.dismissed))
            .unwrap_or(false);
        dismissed && (self.in_session() || self.manual_override())
    }

    /// 设置里的手机屏开关。
    ///
    /// 只改"允许开屏"这个开关；真正开/关由采样线程每秒按会话算（和 C# 一样）。
    /// 关掉时立刻把屏愿望清空，不等下一拍。
    pub fn set_phone_screen(&self, on: bool) {
        self.phone_enabled.store(on, Ordering::Relaxed);
        if !on {
            self.enqueue(|desks| desks.set_wishes(Vec::new()));
        }
    }

    /// 默认字号档位（设置窗的 zoom 滑杆）。
    pub fn set_default_zoom(&self, steps: i32) {
        self.enqueue(move |desks| desks.set_default_zoom(steps));
    }

    /// 把一个 desk 操作排给后台线程（界面线程不在这把锁上等）。
    fn enqueue(&self, action: impl FnOnce(&mut DeskSet) + Send + 'static) {
        match self.pending.lock() {
            Ok(mut queue) => queue.push(Box::new(action)),
            Err(poisoned) => poisoned.into_inner().push(Box::new(action)),
        }
    }

    /// 手动强制保持手机屏；不把它写入真实远程会话的证据。
    pub fn set_manual_override(&self, on: bool) {
        crate::info!("手动强制手机屏 = {on}");
        self.manual_override.store(on, Ordering::Relaxed);
    }

    /// 当前"手动强制"状态（设置窗回显用）。
    pub fn manual_override(&self) -> bool {
        self.manual_override.load(Ordering::Relaxed)
    }

    /// 手机屏的屏幕矩形（物理像素）。界面窗口要搬到这块屏上，对应 C#
    /// `PhoneHomeWindow` 跟着 `_displays.BoundsOf(id)` 走。
    pub fn phone_bounds_of(&self, screen_id: Option<&str>) -> Option<Rect> {
        let id = self.state_of(screen_id).display_id?;
        self.displays.bounds_of(id)
    }

    /// 主屏（非手机屏）的可用区域（物理像素），桌面预览要按它摆放。
    pub fn main_area(&self) -> Option<Rect> {
        self.displays.main_area()
    }

    /// 手机屏底部指示条的区域（物理像素）。有窗口铺在屏上时，我们的界面窗口
    /// 要缩成这一条，对应 C# 的 `HomeBarWindow`（它是独立置顶窗口）。
    pub fn home_bar_area_of(&self, screen_id: Option<&str>) -> Option<Rect> {
        let id = self.state_of(screen_id).display_id?;
        self.displays.home_bar_area_of(id)
    }

    /// 把 `Option<&str>` 屏 id 解析成具体 id（`None` = 主触控屏）。
    fn resolve_screen(&self, screen_id: Option<&str>) -> Option<String> {
        screen_id.map(str::to_string).or_else(|| self.primary_screen_id())
    }

    /// 回到资源库（C# `ShowList`）：界面切回列表，但铺着的窗口**仍留在手机屏上**
    /// （`state.placed` 不清），所以列表里要留一个"回到它"的入口，见 [`Self::return_to_placed`]。
    pub fn show_list_of(&self, screen_id: Option<&str>) {
        if let Some(id) = self.resolve_screen(screen_id) {
            crate::info!("应用列表：请求显示（屏 {id}）");
            self.enqueue(move |desks| desks.show_list(Some(&id)));
        } else {
            crate::warn!("应用列表：当前没有可用屏幕");
        }
    }

    /// 回到手机屏上铺着的那个窗口（C# `ReturnToPlaced`）。
    pub fn return_to_placed_of(&self, screen_id: Option<&str>) {
        if let Some(id) = self.resolve_screen(screen_id) {
            crate::info!("应用列表：返回当前窗口（屏 {id}）");
            self.enqueue(move |desks| desks.return_to_placed(Some(&id)));
        }
    }

    /// 收起手机屏（C# `DismissScreen`）：把铺着的窗口还回去、撤屏，并且**这次远程
    /// 不再自动弹屏**（`dismissed`），要重进得点重入胶囊或托盘菜单。
    pub fn dismiss_screen_of(&self, screen_id: Option<&str>) {
        let Some(id) = self.resolve_screen(screen_id) else {
            return;
        };
        crate::info!("收起手机屏：{id}");
        self.enqueue(move |desks| desks.dismiss_screen(&id));
    }

    /// 取走钩子抛出的选区事件（界面每帧调一次）。
    pub fn take_selection_events(&self) -> Vec<TouchEvent> {
        self.touch_events.take_selection()
    }

    /// 在选区处按一次"复制"（C# `CopySelectionAtAsync`）。
    ///
    /// 走平台层的 `perform_selection_action`：它会先确认前台已经是目标窗口，
    /// 不是就拒发——选区在别人窗口里，乱按 Ctrl+C 是有副作用的。
    pub fn copy_selection_at(&self, point: snug_core::Point) -> bool {
        self.selection_actions
            .perform_selection_action(point, snug_core::selection::SelectionBubbleAction::Copy)
    }

    /// 把某个窗口铺到手机屏上（资源库里点已有窗口）。
    pub fn pull(&self, screen_id: Option<&str>, window: WindowInfo) {
        let Some(id) = self.resolve_screen(screen_id) else {
            crate::warn!("资源库：当前没有屏幕，忽略铺窗");
            return;
        };
        self.enqueue(move |desks| desks.pull(&id, window));
    }

    /// 启动一个 App（资源库里点没在跑的 App）。
    pub fn open(&self, screen_id: Option<&str>, app: AppInfo) {
        let Some(id) = self.resolve_screen(screen_id) else {
            return;
        };
        self.enqueue(move |desks| desks.open(&id, app));
    }

    /// 给一个已经在跑的 App 再开一个窗口（资源库行右侧的「+」）。
    pub fn new_window(&self, screen_id: Option<&str>, app: snug_core::windows::RunningApp) {
        let Some(id) = self.resolve_screen(screen_id) else {
            return;
        };
        self.enqueue(move |desks| desks.new_window(&id, app));
    }

    /// 关掉这个 App 的全部窗口（资源库行右侧的「×」）。
    pub fn quit(&self, screen_id: Option<&str>, app: snug_core::windows::RunningApp) {
        let Some(id) = self.resolve_screen(screen_id) else {
            return;
        };
        self.enqueue(move |desks| desks.quit(&id, app));
    }

    /// 关掉某一个窗口（展开的窗口行右侧的「×」）。
    pub fn close_window(&self, screen_id: Option<&str>, window: WindowInfo) {
        let Some(id) = self.resolve_screen(screen_id) else {
            return;
        };
        self.enqueue(move |desks| desks.close(&id, window));
    }

    /// 手势配置变了，钩子要按新配置重建（C# `SetGesturesEnabled` 等四个 setter + `TouchHook.Options`）。
    pub fn apply_gesture_settings(&self, config: &AppConfig) {
        crate::info!(
            "设置：手势配置更新（总开关={} 滚动={} 长按右键={} 选后复制={} 仅注入={}）",
            config.gestures,
            config.gesture_scroll,
            config.gesture_long_press_right_click,
            config.copy_after_selection,
            config.gestures_injected_only
        );
        // 立刻按新配置重算一次：关掉总开关会当场把钩子摘下来。
        self.sync_touch(config);
    }

    /// 让手势钩子跟手机屏与配置保持一致（C# `UpdateTouchHook`）。
    ///
    /// 每帧调用是安全的：区域/选项没变时什么都不做，所以不会反复装钩子。
    pub fn sync_touch(&self, config: &AppConfig) {
        let state = self.state();
        let region = if config.gestures && state.screen_phase == PhoneScreenPhase::Ready {
            state
                .display_id
                .and_then(|id| self.displays.bounds_of(id))
                .map(|bounds| TouchRegion::for_phone_screen(bounds, state.close_button))
        } else {
            None
        };

        let mut applied = match self.touch_region.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *applied != region {
            let running = self.touch.set_region(region);
            crate::info!(
                "手势钩子：{}（{}）",
                if running { "已接管手机屏" } else { "已停用" },
                match &region {
                    Some(region) => format!("区域 {}×{}", region.frame.width, region.frame.height),
                    None => "手机屏未就绪或手势已关".to_string(),
                }
            );
            *applied = region;
        }
        drop(applied);

        // C#：`AllowScroll = GestureScroll`、`AllowRightClick = GestureLongPressRightClick`、
        // `NotifySelectionEnded = CopyAfterSelection && !ListVisible`。
        self.touch.set_options(TouchOptions {
            allow_scroll: config.gestures && config.gesture_scroll,
            allow_right_click: config.gestures && config.gesture_long_press_right_click,
            notify_selection_ended: config.copy_after_selection && !state.list_visible,
        });
        self.touch
            .set_require_injected_flag(config.gestures_injected_only && !self.manual_override());
    }

    /// 撤屏并停掉后台线程；返回前会把状态机收干净。
    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        if let Ok(mut desks) = self.desks.lock() {
            desks.shutdown();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}
/// 把当前所有屏的状态拷一份给界面线程。
///
/// 后台线程调用；锁只持有到拷完就放掉，界面那边读的是自己的快照，不会等它。
fn publish_snapshots(
    desks: &Arc<std::sync::Mutex<DeskSet>>,
    snapshots: &Arc<std::sync::Mutex<std::collections::HashMap<String, DeskState>>>,
) {
    let states = match desks.lock() {
        Ok(desks) => desks.all_states(),
        Err(_) => return,
    };
    if let Ok(mut snap) = snapshots.lock() {
        *snap = states;
    }
}

/// 当前该有哪些屏——对应 C# v1.3.0 `AppController.OnTick` 的两行：///
/// ```csharp
/// bool remote = _session.InEpisode || _sensor.ManualOverride;
/// _desks.SetWishes(ScreenPlan.Wishes(_config, remote));
/// ```
///
/// 与 1.2 的**关键区别**：不再按控制端类型（手机/平板/电脑）判断要不要开屏。
/// 多设备时代那套启发性判断已经被上游去掉，屏开不开完全由配置里每块屏的
/// `enabled` 决定；总开关（手机屏开关）关掉时一块屏都不要。
///
/// 抽成纯函数是为了能单测：漏掉 `remote` 那一段就是真机上那个
/// "退出远控后虚拟屏还挂着"的 bug。
fn active_wishes(
    config: &AppConfig,
    enabled: bool,
    remote: bool,
    active_screen: Option<&str>,
) -> Vec<snug_core::screen_plan::ScreenWish> {
    if !enabled {
        return Vec::new();
    }
    let mut wishes = snug_core::screen_plan::ScreenPlan::wishes(config, remote, false);
    // 只留当前显示的那块：我们只有一个窗口，多建几块只会得到几块黑屏。
    //
    // `None` 必须和 `Controller::state_of(None)` 选同一块——那边用的是
    // `primary_touch_screen()`（优先竖屏手机），而 `wishes` 是按配置顺序，
    // 两张表不一致时会出现"建的是电脑副屏、界面却在找手机屏"。
    let wanted_id = match active_screen {
        Some(id) => Some(id.to_string()),
        None => config.primary_touch_screen().map(|screen| screen.id),
    };
    match wanted_id {
        Some(id) => wishes.retain(|wish| wish.id == id),
        // 没有触控屏配置（只有电脑屏）：退回配置里的第一块。
        None => wishes.truncate(1),
    }
    wishes
}

#[cfg(test)]
mod tests {
    use super::{UuReading, active_wishes, read_uu_sample};
    use snug_core::AppConfig;

    fn config_with_one_screen() -> AppConfig {
        let mut config = AppConfig::default();
        config.screens = Some(config.effective_screens());
        config
    }

    fn config_with_two_screens() -> AppConfig {
        use snug_core::{DeviceKind, ScreenOrientation};

        let mut config = config_with_one_screen();
        let mut screens = config.effective_screens();
        let mut tablet = screens[0].clone();
        tablet.id = "tablet-1".to_string();
        tablet.name = "iPad".to_string();
        tablet.device.kind = DeviceKind::Tablet;
        tablet.orientation = ScreenOrientation::Landscape;
        screens.push(tablet.sanitize());
        config.screens = Some(screens);
        config
    }

    #[test]
    fn uu_sample_reader_uses_latest_fresh_result_and_expires_old_data() {
        let latest = std::sync::Mutex::new(Some(super::UuSample {
            sampled_at: 4.0,
            signals: snug_win::UuSignalSet::empty(),
        }));
        let mut last = 0.0;
        assert!(matches!(read_uu_sample(&latest, 5.0, &mut last), UuReading::Fresh(_)));
        assert_eq!(last, 4.0);
        assert!(matches!(read_uu_sample(&latest, 6.0, &mut last), UuReading::Pending));
        assert!(matches!(read_uu_sample(&latest, 10.0, &mut last), UuReading::Stale));
    }

    #[test]
    fn uu_sample_reader_does_not_accept_a_result_that_arrived_too_late() {
        let latest = std::sync::Mutex::new(Some(super::UuSample {
            sampled_at: 1.0,
            signals: snug_win::UuSignalSet::empty(),
        }));
        let mut last = 0.0;
        assert!(matches!(read_uu_sample(&latest, 7.0, &mut last), UuReading::Stale));
        assert_eq!(last, 0.0, "stale observation must not reset the expiry clock");
    }

    /// 真机 bug：退出远控后虚拟屏还挂着。根因就是少了 `remote` 这一段。
    #[test]
    fn no_screen_outside_a_session() {
        let config = config_with_one_screen();
        assert!(active_wishes(&config, true, false, None).is_empty());
    }

    #[test]
    fn one_screen_during_a_session() {
        let config = config_with_one_screen();
        assert_eq!(active_wishes(&config, true, true, None).len(), 1);
    }

    #[test]
    fn master_switch_off_wins() {
        let config = config_with_one_screen();
        assert!(active_wishes(&config, false, true, None).is_empty());
    }

    /// 关掉某块屏就不该再有这个愿望。
    #[test]
    fn disabled_screen_drops_its_wish() {
        let mut config = config_with_one_screen();
        let mut screens = config.effective_screens();
        screens[0].enabled = false;
        config.screens = Some(screens);
        assert!(active_wishes(&config, true, true, None).is_empty());
    }

    /// 配了两块屏，但同一时刻只建一块（只画一块屏，多建就是黑屏）：
    /// 不指定时取配置里的第一块。
    #[test]
    fn only_one_screen_is_built_at_a_time() {
        use snug_core::ScreenConfig;

        let config = config_with_two_screens();
        let wishes = active_wishes(&config, true, true, None);
        assert_eq!(wishes.len(), 1, "同一时刻只该建一块屏");
        assert_eq!(wishes[0].id, ScreenConfig::LEGACY_PHONE_ID);
    }

    /// 切到第二块屏：愿望只剩它（旧屏会被撤掉）。
    #[test]
    fn selecting_a_screen_drops_the_other() {
        let config = config_with_two_screens();
        let wishes = active_wishes(&config, true, true, Some("tablet-1"));
        assert_eq!(wishes.len(), 1);
        assert_eq!(wishes[0].id, "tablet-1");
        assert_eq!(wishes[0].fit.kind(), snug_core::DeviceKind::Tablet);
    }

    /// 配置里第一块是电脑屏、第二块才是手机时，默认（没选屏）要建手机屏——
    /// 和 `Controller::state_of(None)` 用的 `primary_touch_screen()` 保持同一块。
    #[test]
    fn default_screen_matches_primary_touch_screen() {
        use snug_core::{DeviceKind, ScreenOrientation};

        let mut config = config_with_one_screen();
        let mut base = config.effective_screens().remove(0);

        let mut computer = base.clone();
        computer.id = "pc-1".to_string();
        computer.name = "PC".to_string();
        computer.device.kind = DeviceKind::Computer;
        computer.orientation = ScreenOrientation::Landscape;

        base.id = "phone-2".to_string();
        base.name = "Phone".to_string();

        config.screens = Some(vec![computer.sanitize(), base.sanitize()]);
        let primary = config.primary_touch_screen().expect("配置里有手机屏");

        let wishes = active_wishes(&config, true, true, None);
        assert_eq!(wishes.len(), 1);
        assert_eq!(wishes[0].id, primary.id, "默认建屏要和 primary_touch_screen 是同一块");
        assert_eq!(wishes[0].fit.kind(), DeviceKind::Phone);
    }
}
