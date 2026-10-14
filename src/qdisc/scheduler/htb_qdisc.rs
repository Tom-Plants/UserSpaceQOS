// htb_qdisc.rs
use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use crate::token_bucket::TokenBucketLimiter;
use std::time::{Duration, Instant};

/// 低优想"借用"高优让出的份额时，要求高优已经**连续这么久没有任何包**才算"没流了"。
/// 为什么不能用瞬时判断：串流的包是一颗一颗到的，包与包之间的空档有 1ms 左右，
/// 瞬时"高优队列为空"永远成立 ⇒ 低优会把高优的空档全偷走（生产实测：串流在跑、
/// 下载却能拿到 ~450KB/s ✗）。用时间窗判断后，高优在跑就凑不满这个窗 ⇒ 借不到 ✓
const HIGH_IDLE_MS: u64 = 200;

pub struct HtbQdisc<T, K, B: TokenBucketLimiter> {
    high_qdisc: Box<dyn Qdisc<T, K>>,
    low_qdisc: Box<dyn Qdisc<T, K>>,
    high_bucket: B,
    low_bucket: B,
    pub global_bucket: B,
    high_reserve: usize,
    low_reserve: usize,
    classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
    last_peek_branch: u8, // 🚀 核心：记录最终决定的提货分支
    /// 最近一次"高优有包"的时刻（None = 还没见过）—— 低优借用前用它判断高优是不是真没流了
    last_high_seen: Option<Instant>,
}

impl<T, K, B: TokenBucketLimiter> HtbQdisc<T, K, B> {
    pub fn new(
        high_qdisc: Box<dyn Qdisc<T, K>>,
        low_qdisc: Box<dyn Qdisc<T, K>>,
        high_bucket: B,
        low_bucket: B,
        global_bucket: B,
        high_reserve: usize,
        low_reserve: usize,
        classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
    ) -> Self {
        Self {
            high_qdisc,
            low_qdisc,
            high_bucket,
            low_bucket,
            global_bucket,
            high_reserve,
            low_reserve,
            classifier,
            last_peek_branch: 0,
            last_high_seen: None,
        }
    }
}

impl<T, K, B> Qdisc<T, K> for HtbQdisc<T, K, B>
where
    B: TokenBucketLimiter,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if (self.classifier)(&ctx) { self.high_qdisc.enqueue(ctx); } 
        else { self.low_qdisc.enqueue(ctx); }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // （曾在这里加过一道"高优连续 200ms 无包才许低优借用"的门 —— 已拆除：
        //  Sunshine 静止时也在持续发包（~1Mbps），那道门等于把低优永久钉死在自己的份额上，
        //  与用户的设计"高优用不掉的部分低优可以吃"相悖。纯 HTB 语义已经够用：
        //  下面四个分支里高优查两轮、且都排在前面，高优的包永远先被服务。）
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                self.last_peek_branch = 1; // 🎯 锁定高优保底
                return self.high_qdisc.peek();
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.low_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                self.last_peek_branch = 2; // 🎯 锁定低优保底
                return self.low_qdisc.peek();
            }
        }
        if let Some(ctx) = self.high_qdisc.peek() {
            let keep = self.low_bucket.burst() as usize; // 同理：实时取低优桶的 burst
            if self.global_bucket.can_spend(ctx.cost + keep) {
                self.last_peek_branch = 3; // 🎯 锁定高优借用
                return self.high_qdisc.peek();
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            // ⚠️ 两个条件缺一不可：
            //   ① high_idle：高优必须"真的没流了"（连续 HIGH_IDLE_MS 没有包）——
            //      这才是"如果高优没流了，低优才可以多吃"的本意。
            //   ② 保留额度**实时**取高优桶当前的 burst（构造时的固定值是按 initial_total_mbps
            //      算的，C 变了它不动 ⇒ 借用支永远不成立）。
            let keep = self.high_bucket.burst() as usize;
            if self.global_bucket.can_spend(ctx.cost + keep) {
                self.last_peek_branch = 4; // 🎯 锁定低优借用
                return self.low_qdisc.peek();
            }
        }
        self.last_peek_branch = 0;
        None
    }

    fn dequeue(&mut self) -> PacketContext<T, K> {
        // 🚀 O(1) 极致提货，再无时间差风波！
        match self.last_peek_branch {
            1 => {
                let real = self.high_qdisc.dequeue();
                self.high_bucket.consume(real.cost);
                self.global_bucket.consume(real.cost);
                real
            }
            2 => {
                let real = self.low_qdisc.dequeue();
                self.low_bucket.consume(real.cost);
                self.global_bucket.consume(real.cost);
                real
            }
            3 => {
                let real = self.high_qdisc.dequeue();
                self.global_bucket.consume(real.cost);
                real
            }
            4 => {
                let real = self.low_qdisc.dequeue();
                self.global_bucket.consume(real.cost);
                real
            }
            _ => unreachable!("致命违规：HTB 在没钱或没包的情况下被强行提货！"),
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek();
        let mut drops = self.high_qdisc.collect_dropped();
        drops.extend(self.low_qdisc.collect_dropped());
        drops
    }
}