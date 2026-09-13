//! 淘汰策略的纯逻辑单测（无 I/O、无线程）。
//!
//! `Replacer` 只保存"顺序/环"这类结构状态；帧的 pin 数与引用位由池通过
//! [`FrameVitals`] 提供。这里的 `MockVitals` 就是那个池的最小替身。
//! 注意：真实的 `BufferPool` 在 `frame_for` 的**命中与载入两条路径**都会置引用位，
//! 假池没有这一步，所以 CLOCK 的用例需要显式 `reference()`。

use std::cell::Cell;
use std::collections::HashMap;

use chibidb::config::EvictionPolicy;
use chibidb::storage::replacer::{FrameVitals, Key, Replacer, from_policy};

const ALL_POLICIES: [EvictionPolicy; 3] =
    [EvictionPolicy::Lru, EvictionPolicy::Clock, EvictionPolicy::Fifo];

#[derive(Default)]
struct MockVitals {
    pins: HashMap<Key, u32>,
    referenced: HashMap<Key, Cell<bool>>,
}

impl MockVitals {
    /// 登记一个常驻且未被 pin 的帧。**未登记的键对策略等同于"已被 pin"**，
    /// 与 `PoolVitals::pins` 对缺失键返回 `u32::MAX` 一致。
    fn resident(&mut self, key: Key) {
        self.pins.insert(key, 0);
        self.referenced.entry(key).or_default();
    }

    fn pin(&mut self, key: Key) {
        self.pins.insert(key, 1);
    }

    fn unpin(&mut self, key: Key) {
        self.pins.insert(key, 0);
    }

    fn reference(&mut self, key: Key) {
        self.referenced.entry(key).or_default().set(true);
    }

    fn is_referenced(&self, key: Key) -> bool {
        self.referenced.get(&key).is_some_and(|c| c.get())
    }
}

impl FrameVitals for MockVitals {
    fn pins(&self, key: Key) -> u32 {
        self.pins.get(&key).copied().unwrap_or(u32::MAX)
    }

    fn referenced(&self, key: Key) -> bool {
        self.referenced.get(&key).is_some_and(|c| c.get())
    }

    fn clear_referenced(&self, key: Key) {
        if let Some(c) = self.referenced.get(&key) {
            c.set(false);
        }
    }
}

const A: Key = (1, 1);
const B: Key = (1, 2);
const C: Key = (1, 3);
const D: Key = (1, 4);

fn pool_with(policy: EvictionPolicy, keys: &[Key]) -> (Box<dyn Replacer>, MockVitals) {
    let mut r = from_policy(policy);
    let mut v = MockVitals::default();
    for &k in keys {
        r.push(k);
        v.resident(k);
    }
    (r, v)
}

#[test]
fn len_tracks_registration() {
    let (mut r, _v) = pool_with(EvictionPolicy::Lru, &[A, B, C]);
    assert_eq!(r.len(), 3);
    r.forget(B);
    assert_eq!(r.len(), 2, "forget 必须与页表同步摘除");
    r.forget(B); // 幂等：再摘一次不应破坏计数
    assert_eq!(r.len(), 2);
}

#[test]
fn lru_evicts_the_least_recently_used() {
    let (mut r, v) = pool_with(EvictionPolicy::Lru, &[A, B, C]);
    r.record_access(A); // A 刚用过 → 顺序变成 [B, C, A]
    assert_eq!(r.choose_victim(&v), Some(B), "B 最久未用");
    assert_eq!(r.len(), 2);
    assert_eq!(r.choose_victim(&v), Some(C), "B 出局后 C 最久未用");
    assert_eq!(r.choose_victim(&v), Some(A), "A 是最后被访问的");
    assert_eq!(r.choose_victim(&v), None, "空了就没人可淘汰");
}

#[test]
fn lru_skips_pinned_frames_and_keeps_them_queued() {
    let (mut r, mut v) = pool_with(EvictionPolicy::Lru, &[A, B, C]);
    v.pin(B); // B 最旧，但被 pin
    assert_eq!(r.choose_victim(&v), Some(A), "跳过被 pin 的 B");
    assert_eq!(r.len(), 2, "被 pin 的帧不能被摘掉登记（不变式 Ⅳ）");
    v.unpin(B);
    assert_eq!(r.choose_victim(&v), Some(B), "B 还在队列里，且仍在队首");
}

#[test]
fn fifo_ignores_hits() {
    let (mut r, v) = pool_with(EvictionPolicy::Fifo, &[A, B]);
    r.record_access(A); // FIFO 不重排
    assert_eq!(r.choose_victim(&v), Some(A), "命中不改变出队顺序");
    assert_eq!(r.choose_victim(&v), Some(B));
    assert_eq!(r.choose_victim(&v), None);
}

#[test]
fn clock_gives_a_second_chance() {
    let (mut r, mut v) = pool_with(EvictionPolicy::Clock, &[A, B]);
    v.reference(A); // A 有第二次机会
    assert_eq!(r.choose_victim(&v), Some(B), "未被引用的 B 先出局");
    assert!(!v.is_referenced(A), "经过 A 时引用位被清零");
    assert_eq!(r.choose_victim(&v), Some(A), "引用位已用掉，这回轮到 A");
    assert_eq!(r.choose_victim(&v), None);
}

#[test]
fn clock_skips_pinned_frames_without_clearing_their_reference_bit() {
    let (mut r, mut v) = pool_with(EvictionPolicy::Clock, &[A, B, C]);
    v.pin(A);
    v.reference(A);
    assert_eq!(r.choose_victim(&v), Some(B), "被 pin 的 A 应被跳过");
    assert!(v.is_referenced(A), "跳过被 pin 的帧时不得消费它的引用位");
    assert_eq!(r.len(), 2);
    v.unpin(A);
    // A 仍带着引用位 → 让位一次；C 未引用，先被淘汰。
    assert_eq!(r.choose_victim(&v), Some(C));
    assert_eq!(r.choose_victim(&v), Some(A));
}

#[test]
fn clock_terminates_when_the_ring_is_only_pinned() {
    let (mut r, mut v) = pool_with(EvictionPolicy::Clock, &[A, B, C, D]);
    for k in [A, B, C, D] {
        v.pin(k);
    }
    assert_eq!(r.choose_victim(&v), None, "全被 pin：必须有界返回 None");
    assert_eq!(r.len(), 4, "返回 None 不得摘掉任何登记");
    // 解除一个 pin，立刻又能选出人来（手仍能推进）。
    v.unpin(C);
    assert_eq!(r.choose_victim(&v), Some(C));
}

#[test]
fn clock_terminates_on_a_mix_of_pinned_and_referenced() {
    let (mut r, mut v) = pool_with(EvictionPolicy::Clock, &[A, B, C, D]);
    v.pin(A);
    v.pin(B);
    v.pin(D);
    v.reference(A); // 被 pin 且被引用：跳过时两者都不该被消耗
    v.reference(C); // 唯一可淘汰的那个还带着引用位
    assert_eq!(r.choose_victim(&v), Some(C), "绕一圈清掉 C 的引用位后淘汰它");
    assert!(v.is_referenced(A), "被 pin 的帧引用位不受影响");
    assert_eq!(r.choose_victim(&v), None, "剩下的都还被 pin");
}

#[test]
fn every_policy_returns_none_when_all_frames_are_pinned() {
    for policy in ALL_POLICIES {
        let (mut r, mut v) = pool_with(policy, &[A, B, C]);
        for k in [A, B, C] {
            v.pin(k);
        }
        assert_eq!(r.choose_victim(&v), None, "{policy:?} 应返回 None");
        assert_eq!(r.len(), 3, "{policy:?} 不得摘掉登记");
    }
}

#[test]
fn the_oldest_frame_goes_first_under_every_policy() {
    // 无 pin、无引用时三条策略对同一个插入序列给出同一个首个 victim。
    for policy in ALL_POLICIES {
        let (mut r, v) = pool_with(policy, &[A, B, C]);
        assert_eq!(r.choose_victim(&v), Some(A), "{policy:?}");
    }
}

#[test]
fn each_policy_evicts_every_frame_exactly_once() {
    for policy in ALL_POLICIES {
        let (mut r, v) = pool_with(policy, &[A, B, C, D]);
        let mut seen = Vec::new();
        while let Some(k) = r.choose_victim(&v) {
            assert_eq!(r.len(), 4 - seen.len() - 1, "{policy:?}: 登记数应同步减少");
            seen.push(k);
        }
        assert_eq!(r.len(), 0, "{policy:?}");
        seen.sort_unstable();
        assert_eq!(seen, vec![A, B, C, D], "{policy:?}: 每个键恰好出局一次");
    }
}
