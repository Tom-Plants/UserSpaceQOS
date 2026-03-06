use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use crate::token_bucket::TokenBucketLimiter;

// ==========================================
// 真正的分层令牌桶调度器 (True HTB Qdisc)
// 核心能力：保底带宽隔离 + 闲置借用 + 🚀 准备金护航
// ==========================================
pub struct HtbQdisc<T, K, B: TokenBucketLimiter> {
    high_qdisc: Box<dyn Qdisc<T, K>>,
    low_qdisc: Box<dyn Qdisc<T, K>>,

    high_bucket: B,
    low_bucket: B,
    pub global_bucket: B,

    high_reserve: usize, // 🚀 新增：只允许 VIP 动用的全局准备金
    low_reserve: usize,  // 🚀 新增：只允许 VIP 动用的全局准备金

    classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
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
        }
    }
}

impl<T, K, B> Qdisc<T, K> for HtbQdisc<T, K, B>
where
    B: TokenBucketLimiter,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if (self.classifier)(&ctx) {
            self.high_qdisc.enqueue(ctx);
        } else {
            self.low_qdisc.enqueue(ctx);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                return self.high_qdisc.peek();
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.low_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                return self.low_qdisc.peek();
            }
        }
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.low_reserve) {
                return self.high_qdisc.peek();
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.high_reserve) {
                return self.low_qdisc.peek();
            }
        }
        None
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 既然 peek 刚确认过，这里重新走一遍分支直接提货扣费即可
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                let real = self.high_qdisc.dequeue()?;
                self.high_bucket.consume(real.cost);
                self.global_bucket.consume(real.cost);
                return Some(real);
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.low_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                let real = self.low_qdisc.dequeue()?;
                self.low_bucket.consume(real.cost);
                self.global_bucket.consume(real.cost);
                return Some(real);
            }
        }
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.low_reserve) {
                let real = self.high_qdisc.dequeue()?;
                self.global_bucket.consume(real.cost);
                return Some(real);
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.high_reserve) {
                let real = self.low_qdisc.dequeue()?;
                self.global_bucket.consume(real.cost);
                return Some(real);
            }
        }
        None
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek(); // 级联触发打扫
        let mut drops = self.high_qdisc.collect_dropped();
        drops.extend(self.low_qdisc.collect_dropped());
        drops
    }
}
