//! 截止期线程用的无锁原语。
//!
//! # 为什么需要这个模块
//!
//! `docs/spec-latency-floor.md` §9.3 把 `jitter_buf` 的 50 ms 判成「唯一还能真正
//! 打开的一大块」，而那 50 ms 买的**不是网络抖动**（实测 p95 0.18 ms、丢包 0），
//! 是 `tx_loop` 自己的调度停顿尾。决定性对照：一条与 `tx_loop` 的线程 QoS 与
//! 等待机制逐字同构的独立探针，mac 上 60000 tick 里 **≥30 ms 的迟到为 0**、
//! 最大 27.39 ms；而 daemon 自己在 20 ms 深度的欠载率折算成每 tick 6.3e-4，
//! 探针同深度只有 5.0e-5 —— **daemon 的尾比裸探针肥 12.5 倍**。
//!
//! 那 12.5 倍是循环体自己招来的。本模块提供把它们搬走所需的那一件东西：
//! 一条**生产者永不阻塞、永不分配、永不进内核**的交接通道。
//!
//! # 为什么不用 `Mutex<VecDeque>` / `std::sync::mpsc`
//!
//! - `Mutex`：临界区本身只有几十纳秒，但**持有者被抢占**时等待方要陪等一个调度
//!   量子。那正是我们在追的那条尾——把 `sendto` 从这条线程上搬走、再引入一把
//!   会被别的普通优先级线程持有的锁，等于换了个地方长出同一条尾。
//! - `std::sync::mpsc`：现今是 crossbeam 的链表通道，每约 32 条消息**分配一个
//!   block**。1600 pps 下就是 50 次/秒的 `malloc`，而「每 tick 的 `Vec` 分配」
//!   正是本轮要消灭的第 3 项。
//!
//! 所以是定长槽 + 两个下标的 Lamport 环：稳态下生产者只做「两次原子读 + 就地
//! 填充 + 一次 Release store」，没有分配、没有系统调用、没有锁。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// 单生产者单消费者的定长环。
///
/// # 契约（违反即 UB，也是 `unsafe` 的全部依据）
///
/// - **恰好一个**线程调用 [`SpscRing::produce`]；
/// - **恰好一个**线程调用 [`SpscRing::consume`]；
/// - 槽里的 `T` 在建环时一次性造好，之后只被**就地**改写，永不移动、永不重建。
///
/// # 为什么这样是安全的
///
/// 生产者只在 `write − read < cap` 时触碰下标 `write & mask` 的槽；消费者只在
/// `read != write` 时触碰 `read & mask`。两个区间在 `write − read < cap` 这个
/// 前提下不相交，所以任一时刻一个槽至多被一个线程持有可变引用。
/// 跨线程可见性由两次 Release/Acquire 配对给出：
/// `write` 的 Release store 把「槽已填好」发布给消费者的 Acquire load，
/// `read` 的 Release store 把「槽已用完」发布给生产者的 Acquire load。
///
/// 容量强制为 **2 的幂**：下标是单调递增的 `usize`，只有 2 的幂才让
/// `idx & mask` 在 `usize` 回绕时仍然连续（`usize::MAX + 1` 是 2 的幂的倍数）。
/// 非 2 的幂用 `%` 会在回绕那一刻错位一次——一次，几百年后，在生产环境里，
/// 而且没有任何东西会报错。这就是它必须是编译期约束而不是文档约定的原因。
pub(crate) struct SpscRing<T> {
    slots: Box<[UnsafeCell<T>]>,
    mask: usize,
    /// 只由生产者写。
    write: AtomicUsize,
    /// 只由消费者写。
    read: AtomicUsize,
    /// 环满时被**拒收**的次数。只由生产者写。
    ///
    /// 语义是「生产者来了一条、环满了、没收下」。调用方不重试时它就等于
    /// 丢弃数（`UdpSender::enqueue` 正是如此，见那里）；调用方自旋重试时
    /// 它是「撞满了几次」。两种读法都要，所以名字里不写死 loss。
    rejected: AtomicU64,
}

// SAFETY: 见类型文档的契约。`T: Send` 是因为槽的内容会在生产者线程上构造、
// 在消费者线程上被读取与改写。
unsafe impl<T: Send> Send for SpscRing<T> {}
unsafe impl<T: Send> Sync for SpscRing<T> {}

impl<T> SpscRing<T> {
    /// 造一个容量为 `cap`（必须是 2 的幂）的环，每个槽用 `make(i)` 预先造好。
    ///
    /// **全部分配都发生在这里**，之后的稳态路径一次都不分配。
    pub(crate) fn new(cap: usize, mut make: impl FnMut(usize) -> T) -> SpscRing<T> {
        assert!(cap.is_power_of_two() && cap >= 2, "容量必须是 ≥2 的 2 的幂，给的是 {cap}");
        let slots: Vec<UnsafeCell<T>> = (0..cap).map(|i| UnsafeCell::new(make(i))).collect();
        SpscRing {
            slots: slots.into_boxed_slice(),
            mask: cap - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// 生产一条。`fill` 就地改写槽；返回 `false` 表示**这一条作废**（例如封包
    /// 失败），此时下标不推进、消费者永远看不到它。
    ///
    /// 环满时返回 `false` 并计一次丢弃。**不阻塞、不分配、不进内核**。
    pub(crate) fn produce(&self, fill: impl FnOnce(&mut T) -> bool) -> bool {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= self.capacity() {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: `w − r < cap` ⇒ 消费者此刻只可能持有 `[r, w)` 里的某一个槽，
        // 而我们要写的是 `w`，不在那个区间里。
        let slot = unsafe { &mut *self.slots[w & self.mask].get() };
        if !fill(slot) {
            return false;
        }
        self.write.store(w.wrapping_add(1), Ordering::Release);
        true
    }

    /// 消费最旧的一条。`take` 就地读取（并可就地清理，例如把槽里的 `Arc` 取走
    /// 让它在**本线程**析构）。返回 `false` 表示环空。
    pub(crate) fn consume(&self, take: impl FnOnce(&mut T)) -> bool {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        if r == w {
            return false;
        }
        // SAFETY: `r != w` ⇒ 生产者此刻只可能持有 `w`，不是 `r`。
        let slot = unsafe { &mut *self.slots[r & self.mask].get() };
        take(slot);
        self.read.store(r.wrapping_add(1), Ordering::Release);
        true
    }

    /// 此刻排着的条目数（近似：并发下是瞬时值）。
    pub(crate) fn len(&self) -> usize {
        self.write
            .load(Ordering::Relaxed)
            .wrapping_sub(self.read.load(Ordering::Relaxed))
    }

    /// 累计因环满被拒收的次数。见字段文档。
    pub(crate) fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_full_ring_drops_instead_of_blocking() {
        let r: SpscRing<u32> = SpscRing::new(4, |_| 0);
        for i in 0..4u32 {
            assert!(r.produce(|s| {
                *s = i;
                true
            }));
        }
        assert_eq!(r.len(), 4);
        // 第 5 条：不阻塞、不扩容，计一次丢弃。
        assert!(!r.produce(|s| {
            *s = 99;
            true
        }));
        assert_eq!(r.rejected(), 1, "环满没有计数 —— 丢弃就成了新的观测盲区");
        // 消费一条之后又能放进去了。
        let mut got = 0;
        assert!(r.consume(|s| got = *s));
        assert_eq!(got, 0, "先进先出");
        assert!(r.produce(|s| {
            *s = 4;
            true
        }));
    }

    /// `fill` 返回 false 的那一条**不许**被消费者看到（封包失败走这条路）。
    #[test]
    fn a_rejected_fill_never_becomes_visible() {
        let r: SpscRing<u32> = SpscRing::new(2, |_| 0);
        assert!(!r.produce(|s| {
            *s = 7;
            false
        }));
        assert_eq!(r.len(), 0);
        assert!(!r.consume(|_| panic!("作废的条目被消费了")));
        assert_eq!(r.rejected(), 0, "作废不是拒收，两者语义不同");
    }

    /// 跨线程的 FIFO 与不丢不重。回绕也一起测到（跑的条目数 ≫ 容量）。
    #[test]
    fn producer_and_consumer_agree_across_threads() {
        const N: u32 = 200_000;
        let r: Arc<SpscRing<u32>> = Arc::new(SpscRing::new(64, |_| 0));
        let rc = r.clone();
        let consumer = std::thread::spawn(move || {
            let mut next = 0u32;
            let mut spins = 0u64;
            while next < N {
                let mut got = None;
                if rc.consume(|s| got = Some(*s)) {
                    assert_eq!(got, Some(next), "顺序错了或丢了一条");
                    next += 1;
                    spins = 0;
                } else {
                    spins += 1;
                    assert!(spins < 100_000_000, "生产者停了");
                    std::hint::spin_loop();
                }
            }
            next
        });
        let mut i = 0u32;
        while i < N {
            if r.produce(|s| {
                *s = i;
                true
            }) {
                i += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        assert_eq!(consumer.join().unwrap(), N);
        // 生产者自旋重试，所以**一条都不许少**；`rejected` 会是正数（撞满过
        // 很多次），那不是丢失 —— 这正是这个计数器不叫 `dropped` 的理由。
        assert!(r.rejected() > 0, "64 槽跑 20 万条却一次都没撞满？测试没在真的并发");
    }
}
