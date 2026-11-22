//! 串行队列，对应 C# 类 `SnugWin.Core.PhoneScreen.SerialLane`。
//!
//! C# 版用"任务接龙"（`_tail` 串起 `Task`）保证所有桌面动作按序执行，且前一个出错不影响
//! 后一个。Rust 版换成"一条工作线程 + mpsc 队列"：动作按入队顺序逐个执行，
//! 被移动的状态由工作线程独占，界面线程通过队列提交动作、从共享快照读状态。
//!
//! 与 C# 一样是"排队执行"，不是"调用即执行"：公开方法只入队，不等完成。
//!
//! 与 C# 的一处差别：C# 的 `ExecuteAsync` 会吞掉前一个任务的异常继续跑，
//! 这里不吞 panic——动作里 panic 说明是代码 bug，工作线程随之退出（测试会直接暴露），
//! 不做"带病继续"的掩盖。

use std::io::Write;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

/// 单个动作排队或执行超过这个时长就写一行 stderr。
///
/// 界面里"点了没反应"最常见的原因就是队列积压或单个动作过慢（改显示配置、枚举窗口），
/// 正常动作都是毫秒级，超过 3 秒一定有问题。真机联调时把 stderr 重定向到文件即可看到。
const SLOW_THRESHOLD: f64 = 3.0;

/// 启动终端退出后 stderr 可能已经断开；诊断输出失败不能终止桌面工作线程。
fn write_diagnostic(writer: &mut impl Write, message: std::fmt::Arguments<'_>) {
    let _ = writeln!(writer, "{message}");
}

enum LaneJob<T> {
    Run {
        run: Box<dyn FnOnce(&mut T) + Send>,
        /// `call()` 用来等这一条执行完；`run()` 不填。
        done: Option<Sender<()>>,
        /// 入队时刻：执行时算"排了多久"，真机调试队列积压用。
        queued_at: std::time::Instant,
        /// 入队处的源码位置：慢动作日志靠它指名道姓。
        site: &'static std::panic::Location<'static>,
    },
    /// 关闭工作线程。桌面状态自己攥着发送端，靠这条消息才能停下来。
    Stop,
}

pub(crate) struct SerialLane<T: Send + 'static> {
    tx: Sender<LaneJob<T>>,
}

/// 把源码位置缩成 `engine.rs:1248` 这种短形式（完整路径太长，stderr 里看不清）。
fn where_of(site: &'static std::panic::Location<'static>) -> String {
    let file = site.file();
    let short = file.rsplit(['/', '\\']).next().unwrap_or(file);
    format!("{short}:{}", site.line())
}

impl<T: Send + 'static> Clone for SerialLane<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl<T: Send + 'static> SerialLane<T> {
    /// 起工作线程。`make_state` 可以用传进来的 lane 构造状态（状态需要反向投递时用）。
    ///
    /// 工作线程是"分离"的：句柄被丢掉，队列关闭或收到 [`Self::stop`] 后自行退出。
    pub(crate) fn start(make_state: impl FnOnce(Self) -> T) -> Self {
        let (tx, rx) = channel::<LaneJob<T>>();
        let lane = Self { tx };
        let state = make_state(lane.clone());
        let _worker: JoinHandle<()> = std::thread::Builder::new()
            .name("snug-desk".to_string())
            .spawn(move || {
                let mut state = state;
                while let Ok(job) = rx.recv() {
                    match job {
                        LaneJob::Run { run, done, queued_at, site } => {
                            // 队列积压/动作过慢是"界面点了没反应"的头号嫌疑，超过阈值
                            // 就写一行 stderr（真机联调时把 stderr 重定向到文件看）。
                            // 带上入队处的源码位置，否则只知道"有个动作慢了 3 秒"，
                            // 还得靠时间戳猜是哪一个。
                            let waited = queued_at.elapsed().as_secs_f64();
                            if waited > SLOW_THRESHOLD {
                                write_diagnostic(
                                    &mut std::io::stderr().lock(),
                                    format_args!("[lane] 动作排队 {waited:.1}s 才轮到（{}）", where_of(site)),
                                );
                            }
                            let started = std::time::Instant::now();
                            run(&mut state);
                            let took = started.elapsed().as_secs_f64();
                            if took > SLOW_THRESHOLD {
                                write_diagnostic(
                                    &mut std::io::stderr().lock(),
                                    format_args!("[lane] 单个动作执行了 {took:.1}s（{}）", where_of(site)),
                                );
                            }
                            if let Some(done) = done {
                                let _ = done.send(());
                            }
                        }
                        LaneJob::Stop => break,
                    }
                }
            })
            .expect("创建 PhoneDesk 工作线程失败");
        lane
    }

    /// 等价于给 `start` 传一个现成状态（测试用）。
    #[cfg(test)]
    pub(crate) fn spawn(state: T) -> Self {
        Self::start(|_| state)
    }

    /// 入队一个动作，不等它执行（对应 C# `_lane.Run`）。
    #[track_caller]
    pub(crate) fn run(&self, operation: impl FnOnce(&mut T) + Send + 'static) {
        self.run_at(std::panic::Location::caller(), operation);
    }

    /// 同 [`Self::run`]，但入队位置由调用方给（需要把位置继续往下传时用）。
    pub(crate) fn run_at(
        &self,
        site: &'static std::panic::Location<'static>,
        operation: impl FnOnce(&mut T) + Send + 'static,
    ) {
        let _ = self.tx.send(LaneJob::Run {
            run: Box::new(operation),
            done: None,
            queued_at: std::time::Instant::now(),
            site,
        });
    }

    /// 入队并等它执行完；供 watch 线程等一次桌面动作返回。
    ///
    /// 不要在队列里的动作内部调用，否则工作线程会等自己（死锁）。
    ///
    /// 生产路径一律走 [`Self::call_at`]（`DeskSlot` 会把源码位置继续传下来），
    /// 这里只留给测试。
    #[cfg(test)]
    #[track_caller]
    pub(crate) fn call(&self, operation: impl FnOnce(&mut T) + Send + 'static) {
        self.call_at(std::panic::Location::caller(), operation);
    }

    /// 同 [`Self::call`]，但入队位置由调用方给。
    pub(crate) fn call_at(
        &self,
        site: &'static std::panic::Location<'static>,
        operation: impl FnOnce(&mut T) + Send + 'static,
    ) {
        let (done, wait) = channel();
        if self
            .tx
            .send(LaneJob::Run {
                run: Box::new(operation),
                done: Some(done),
                queued_at: std::time::Instant::now(),
                site,
            })
            .is_err()
        {
            // 工作线程已经退出，不再等待。
            return;
        }
        let _ = wait.recv();
    }

    /// 入队并等它执行完，最多等 `timeout`；返回是否真的执行完了。
    ///
    /// 给**收尾**用：退出时不能无限等在飞的长动作（一次建屏 = 8 秒 × 候选数，
    /// 后面还可能跟着最短存活 2.5 秒与拓扑沉降 1 秒）。超时说明这条动作没跑完，
    /// 调用方要把"没撤干净"记进日志；虚拟屏由归属记录在下次启动时兜底清理。
    ///
    /// 位置由调用方给（`DeskSlot` 会把源码位置传下来），所以没有 `#[track_caller]` 版本。
    pub(crate) fn call_timeout_at(
        &self,
        site: &'static std::panic::Location<'static>,
        timeout: std::time::Duration,
        operation: impl FnOnce(&mut T) + Send + 'static,
    ) -> bool {
        let (done, wait) = channel();
        if self
            .tx
            .send(LaneJob::Run {
                run: Box::new(operation),
                done: Some(done),
                queued_at: std::time::Instant::now(),
                site,
            })
            .is_err()
        {
            return false;
        }
        wait.recv_timeout(timeout).is_ok()
    }

    /// 等此前入队的动作全部执行完（对应 C# `DrainAsync`）。
    ///
    /// 只给测试和 `idle()` 用：生产路径不需要等队列（等它就会把界面和状态更新
    /// 一起卡住，见 `docs/desk-threading.md`）。
    #[cfg(test)]
    pub(crate) fn drain(&self) {
        self.call(|_| {});
    }

    /// 关掉工作线程（以及这条队列上还没跑的动作）。
    pub(crate) fn stop(&self) {
        let _ = self.tx.send(LaneJob::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn jobs_run_in_enqueue_order() {
        let lane = SerialLane::spawn(Vec::<u32>::new());
        for i in 0..100u32 {
            lane.run(move |state: &mut Vec<u32>| state.push(i));
        }
        lane.drain();
        let seen = Arc::new(AtomicUsize::new(0));
        let sink = seen.clone();
        lane.call(move |state: &mut Vec<u32>| {
            assert_eq!(state.len(), 100);
            assert!(state.windows(2).all(|w| w[0] < w[1]), "顺序错乱：{:?}", state);
            sink.store(state.len(), Ordering::SeqCst);
        });
        assert_eq!(seen.load(Ordering::SeqCst), 100);
        lane.stop();
    }

    #[test]
    fn broken_diagnostic_pipe_does_not_stop_later_jobs() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lane = SerialLane::spawn(0u32);
        lane.run(|state| {
            write_diagnostic(&mut ClosedPipe, format_args!("slow diagnostic"));
            *state += 1;
        });
        let (done, received) = channel();
        lane.run(move |state| {
            *state += 1;
            let _ = done.send(*state);
        });
        assert_eq!(received.recv_timeout(std::time::Duration::from_secs(2)).unwrap(), 2);
        lane.stop();
    }

    #[test]
    fn call_blocks_until_the_job_has_run() {
        let lane = SerialLane::spawn(0u32);
        lane.run(|state: &mut u32| *state += 1);
        let observed = Arc::new(AtomicUsize::new(0));
        let sink = observed.clone();
        lane.call(move |state: &mut u32| {
            *state += 10;
            sink.store(*state as usize, Ordering::SeqCst);
        });
        assert_eq!(observed.load(Ordering::SeqCst), 11, "call 返回时动作必须已经执行");
        lane.stop();
    }

    #[test]
    fn drain_waits_for_previously_enqueued_work() {
        let lane = SerialLane::spawn(0u32);
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..50 {
            let done = done.clone();
            // 故意让每个动作稍微花点时间，确保 drain 真的在等。
            lane.run(move |state: &mut u32| {
                std::thread::sleep(std::time::Duration::from_micros(200));
                *state += 1;
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
        lane.drain();
        assert_eq!(done.load(Ordering::SeqCst), 50);
        lane.stop();
    }

    #[test]
    fn stop_runs_after_queued_work_and_ends_the_worker() {
        let lane = SerialLane::spawn(0u32);
        let (done, arrived) = channel::<u32>();
        lane.run(move |state: &mut u32| {
            *state += 1;
            let _ = done.send(*state);
        });
        lane.stop();
        // stop 之前排队的动作先跑完：这里能收到就是证明。
        assert_eq!(arrived.recv().ok(), Some(1));
        // stop 之后再投递也不会执行（队列已关）。
        let (late, late_rx) = channel::<()>();
        lane.run(move |_state: &mut u32| {
            let _ = late.send(());
        });
        assert!(late_rx.try_recv().is_err(), "队列关闭后不应再执行动作");
    }
}
