//! 缓冲池淘汰策略接缝：`lru`（默认）/ `clock` / `fifo`。
//!
//! 策略只保存"顺序 / 环"这类**结构状态**；帧上的 pin 计数与引用位由池通过
//! [`FrameVitals`] 提供 —— 单一真源，避免两处状态漂移。
//! 每个池持有一个淘汰器，且**总是在 `state` 锁内**被调用，因此实现内部不加锁。
//!
//! 不变式 Ⅳ 的"登记"一侧由本模块维护：`len()` 必须恒等于池的 `frames.len()`。
//! pin **不是**注销登记，只是取消候选资格 —— 被 pin 的帧仍留在队列/环里。

use std::collections::VecDeque;

use crate::config::EvictionPolicy;
use crate::storage::page::{FileId, PageNo};

/// 页在池中的键。
pub type Key = (FileId, PageNo);

/// 淘汰器向池查询帧状态的最小接口。
///
/// `pins` 对**不在页表中**的键返回 `u32::MAX`：防御性的保守取值，
/// 即使调用方违反了"策略与页表同步"的前提，也只会少淘汰而不会选出不存在的帧。
pub trait FrameVitals {
    /// 该帧当前的 pin 数（0 = 可淘汰）。
    fn pins(&self, key: Key) -> u32;
    /// CLOCK 的引用位。只有 CLOCK 会读它。
    fn referenced(&self, key: Key) -> bool;
    /// 清引用位（CLOCK 的"第二次机会"）。
    fn clear_referenced(&self, key: Key);
}

/// 淘汰策略。每个池一个实例，归 `PoolState` 所有。
pub trait Replacer: Send {
    /// 登记一个刚载入的帧（"最新"端）。
    fn push(&mut self, key: Key);
    /// 命中通知：LRU 重排，FIFO / CLOCK 空实现。仅对**已登记**的键调用。
    fn record_access(&mut self, key: Key);
    /// 摘除一个键（淘汰或 `discard_file`）。键不在时必须幂等。
    fn forget(&mut self, key: Key);
    /// 取出下一个淘汰对象的键，只考虑 `pins == 0` 的帧。
    /// 返回 `None` 当且仅当所有登记帧都被 pin。
    fn choose_victim(&mut self, vitals: &dyn FrameVitals) -> Option<Key>;
    /// 登记键数；必须恒等于池的 `frames.len()`。
    fn len(&self) -> usize;

    /// 只在 `debug_assert!` 里用得到，默认实现足矣。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 按配置构造策略。
pub fn from_policy(policy: EvictionPolicy) -> Box<dyn Replacer> {
    match policy {
        EvictionPolicy::Lru => Box::<Lru>::default(),
        EvictionPolicy::Clock => Box::<Clock>::default(),
        EvictionPolicy::Fifo => Box::<Fifo>::default(),
    }
}

/// 从队首找第一个未 pin 的键并**只摘掉它**。
///
/// 被 pin 的键留在队列里，否则它们会丢掉淘汰顺序（不变式 Ⅳ 要求登记数
/// 恒等于页表长度，所以不能像旧的 `pop_front` 那样把前缀一并丢掉）。
fn pick_oldest(order: &mut VecDeque<Key>, vitals: &dyn FrameVitals) -> Option<Key> {
    let pos = order.iter().position(|&k| vitals.pins(k) == 0)?;
    order.remove(pos)
}

/// 摘掉一个键的登记；不在则什么也不做。
fn drop_key(order: &mut VecDeque<Key>, key: Key) {
    if let Some(pos) = order.iter().position(|&k| k == key) {
        order.remove(pos);
    }
}

/// LRU：队首最久未用。与引入本接缝之前的 `VecDeque` + `touch` 行为逐位等价。
#[derive(Default)]
pub struct Lru {
    order: VecDeque<Key>,
}

impl Replacer for Lru {
    fn push(&mut self, key: Key) {
        self.order.push_back(key);
    }

    fn record_access(&mut self, key: Key) {
        // 线性扫描：本接缝的价值在"换出策略可替换"，不在复杂度；容量到上千时
        // 这是明确的债（HANDOFF §8），要 O(1) 得换成索引结构。
        let Some(pos) = self.order.iter().position(|&k| k == key) else {
            debug_assert!(false, "record_access on an unregistered key");
            return;
        };
        self.order.remove(pos);
        self.order.push_back(key);
    }

    fn forget(&mut self, key: Key) {
        drop_key(&mut self.order, key);
    }

    fn choose_victim(&mut self, vitals: &dyn FrameVitals) -> Option<Key> {
        pick_oldest(&mut self.order, vitals)
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

/// FIFO：命中**不**重排 —— 与 LRU 的唯一差别。
#[derive(Default)]
pub struct Fifo {
    order: VecDeque<Key>,
}

impl Replacer for Fifo {
    fn push(&mut self, key: Key) {
        self.order.push_back(key);
    }

    fn record_access(&mut self, _key: Key) {}

    fn forget(&mut self, key: Key) {
        drop_key(&mut self.order, key);
    }

    fn choose_victim(&mut self, vitals: &dyn FrameVitals) -> Option<Key> {
        pick_oldest(&mut self.order, vitals)
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

/// CLOCK（二次机会）：一个环加一只手。引用位不在这里，而是帧上的
/// `Frame::accessed` —— 省一次哈希，也避免两处状态。
#[derive(Default)]
pub struct Clock {
    ring: Vec<Key>,
    hand: usize,
}

impl Replacer for Clock {
    fn push(&mut self, key: Key) {
        self.ring.push(key);
    }

    fn record_access(&mut self, _key: Key) {}

    fn forget(&mut self, key: Key) {
        let Some(pos) = self.ring.iter().position(|&k| k == key) else {
            return;
        };
        // 用 `remove` 而不是 `swap_remove`：保持环序稳定，手后面的键不会被打乱。
        self.ring.remove(pos);
        if self.hand >= self.ring.len() {
            self.hand = 0;
        }
    }

    fn choose_victim(&mut self, vitals: &dyn FrameVitals) -> Option<Key> {
        let n = self.ring.len();
        if n == 0 {
            return None;
        }
        // 上界 2n+1 保证终止：第一趟最多清掉 n 个引用位，第二趟必然遇到一个
        // 未引用的帧；全被 pin 时循环耗尽并返回 None，而不是挂住。
        for _ in 0..2 * n + 1 {
            if self.hand >= n {
                self.hand = 0;
            }
            let key = self.ring[self.hand];
            if vitals.pins(key) > 0 {
                // 被 pin：跳过，且**不消耗**它的引用位 —— 下一轮再议。
                self.hand += 1;
                continue;
            }
            if vitals.referenced(key) {
                vitals.clear_referenced(key); // 第二次机会
                self.hand += 1;
                continue;
            }
            self.ring.remove(self.hand);
            if self.hand >= self.ring.len() {
                self.hand = 0;
            }
            return Some(key);
        }
        None
    }

    fn len(&self) -> usize {
        self.ring.len()
    }
}
