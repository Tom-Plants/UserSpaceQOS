// htb_qdisc.rs
use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use crate::token_bucket::TokenBucketLimiter;

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
            last_peek_branch: 0
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
            if self.global_bucket.can_spend(ctx.cost + self.low_reserve) {
                self.last_peek_branch = 3; // 🎯 锁定高优借用
                return self.high_qdisc.peek();
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.high_reserve) {
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