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

use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

struct LaneJob<T> {
    run: Box<dyn FnOnce(&mut T) + Send>,
    /// `call()` 用来等这一条执行完；`run()` 不填。
    done: Option<Sender<()>>,
}

pub(crate) struct SerialLane<T: Send + 'static> {
    tx: Sender<LaneJob<T>>,
}

impl<T: Send + 'static> Clone for SerialLane<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl<T: Send + 'static> SerialLane<T> {
    /// 起工作线程并把状态交给它独占。
    pub(crate) fn spawn(state: T) -> (Self, JoinHandle<()>) {
        let (tx, rx) = channel::<LaneJob<T>>();
        let handle = std::thread::Builder::new()
            .name("snug-desk".to_string())
            .spawn(move || {
                let mut state = state;
                // 所有发送端都消失后队列关闭，线程退出。
                while let Ok(job) = rx.recv() {
                    (job.run)(&mut state);
                    if let Some(done) = job.done {
                        let _ = done.send(());
                    }
                }
            })
            .expect("创建 PhoneDesk 工作线程失败");
        (Self { tx }, handle)
    }

    /// 入队一个动作，不等它执行（对应 C# `_lane.Run`）。
    pub(crate) fn run(&self, operation: impl FnOnce(&mut T) + Send + 'static) {
        let _ = self.tx.send(LaneJob { run: Box::new(operation), done: None });
    }

    /// 入队并等它执行完；供 watch 线程等一次桌面动作返回。
    ///
    /// 不要在队列里的动作内部调用，否则工作线程会等自己（死锁）。
    pub(crate) fn call(&self, operation: impl FnOnce(&mut T) + Send + 'static) {
        let (done, wait) = channel();
        if self.tx.send(LaneJob { run: Box::new(operation), done: Some(done) }).is_err() {
            // 工作线程已经退出，不再等待。
            return;
        }
        let _ = wait.recv();
    }

    /// 等此前入队的动作全部执行完（对应 C# `DrainAsync`）。
    pub(crate) fn drain(&self) {
        self.call(|_| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn jobs_run_in_enqueue_order() {
        let (lane, handle) = SerialLane::spawn(Vec::<u32>::new());
        for i in 0..100u32 {
            lane.run(move |state: &mut Vec<u32>| state.push(i));
        }
        lane.drain();
        let seen = Arc::new(AtomicUsize::new(0));
        let seen2 = seen.clone();
        lane.call(move |state: &mut Vec<u32>| {
            assert_eq!(state.len(), 100);
            assert!(state.windows(2).all(|w| w[0] < w[1]), "顺序错乱：{:?}", state);
            seen2.store(state.len(), Ordering::SeqCst);
        });
        assert_eq!(seen.load(Ordering::SeqCst), 100);
        drop(lane);
        handle.join().expect("工作线程应正常退出");
    }

    #[test]
    fn call_blocks_until_the_job_has_run() {
        let (lane, handle) = SerialLane::spawn(0u32);
        lane.run(|state: &mut u32| *state += 1);
        let observed = Arc::new(AtomicUsize::new(0));
        let sink = observed.clone();
        lane.call(move |state: &mut u32| {
            *state += 10;
            sink.store(*state as usize, Ordering::SeqCst);
        });
        assert_eq!(observed.load(Ordering::SeqCst), 11, "call 返回时动作必须已经执行");
        drop(lane);
        handle.join().expect("join");
    }

    #[test]
    fn drain_waits_for_previously_enqueued_work() {
        let (lane, handle) = SerialLane::spawn(0u32);
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
        drop(lane);
        handle.join().expect("join");
    }

    #[test]
    fn worker_stops_when_every_sender_is_gone() {
        let (lane, handle) = SerialLane::spawn(0u32);
        let alias = lane.clone();
        alias.run(|state: &mut u32| *state += 1);
        drop(lane);
        drop(alias);
        handle.join().expect("发送端全部释放后工作线程应退出");
    }
}
