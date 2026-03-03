use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use crate::token_bucket::TokenBucketLimiter;

// ==========================================
// 绝对防弹的限速器 (RateLimitQdisc / TBF)
// 引入动态备用金 (Reserved Tokens) 机制防饥饿
// ==========================================
pub struct RateLimitQdisc<T, K, TB> {
    pub inner: Box<dyn Qdisc<T, K>>,
    pub bucket: TB,
    // 🚀 核心新增：基于包特征的“备用金”评估器
    pub reserve_fn: Box<dyn Fn(&PacketContext<T, K>) -> usize>,
}

impl<T, K, TB> RateLimitQdisc<T, K, TB> {
    pub fn new(
        inner: Box<dyn Qdisc<T, K>>,
        bucket: TB,
        reserve_fn: Box<dyn Fn(&PacketContext<T, K>) -> usize>, // 注入闭包
    ) -> Self {
        Self {
            inner,
            bucket,
            reserve_fn,
        }
    }
}

impl<T, K, TB> Qdisc<T, K> for RateLimitQdisc<T, K, TB>
where
    TB: TokenBucketLimiter,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        self.inner.enqueue(ctx)
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if let Some(ctx) = self.inner.peek() {
            // 🚀 核心逻辑 1：计算这个包的提款门槛
            let reserve_bytes = (self.reserve_fn)(ctx);

            // 桶里的钱，必须大于等于：包的真实运费 + 强制保留的备用金
            if self.bucket.can_spend(ctx.cost + reserve_bytes) {
                return Some(ctx);
            }
        }
        None
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        if let Some(ctx) = self.inner.peek() {
            let reserve_bytes = (self.reserve_fn)(ctx);

            // 再次确认：钱够不够运费 + 备用金
            if self.bucket.can_spend(ctx.cost + reserve_bytes) {
                if let Some(real_ctx) = self.inner.dequeue() {
                    // 🚀 核心逻辑 2：真实扣费时，只扣真实运费！
                    // 备用金只是“门槛”，不需要真的花掉它
                    self.bucket.consume(real_ctx.cost);
                    return Some(real_ctx);
                }
            }
        }
        None
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        self.inner.collect_dropped()
    }
}
