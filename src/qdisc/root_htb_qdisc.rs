use crate::{packet_context::PacketContext, qdisc::Qdisc, token_bucket::TokenBucketLimiter};

// ==========================================
// 1. 依赖的接口定义 (包含你现有的 Qdisc 和 面单)
// ==========================================

// 路由标签（入场券）
// pub enum TrafficClass<HighParam, LowParam> {
//     High(HighParam),
//     Default(LowParam),
// }

// ==========================================
// 2. RootHtbQdisc 核心实现
// ==========================================

pub struct RootHtbQdisc<T, K, GlobalBucket, HighBucket> {
    high_queue: Box<dyn Qdisc<T, K>>,    // ✅ 彻底替换掉 VecDeque
    default_queue: Box<dyn Qdisc<T, K>>, // 原来的 Inner 改名叫 LowInner 方便区分
    pub global_bucket: GlobalBucket,
    pub high_bucket: HighBucket,
    reserved_bytes: usize,
    // 🗑️ 删除了 max_high_len，因为现在队列的最大长度由 HighInner 内部自己管理！
    classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
}

impl<T, K, GlobalBucket, HighBucket> RootHtbQdisc<T, K, GlobalBucket, HighBucket>
where
    GlobalBucket: TokenBucketLimiter,
    HighBucket: TokenBucketLimiter,
{
    pub fn new(
        high_queue: Box<dyn Qdisc<T, K>>, // ✅ 初始化时，直接把装配好的高优底层队列传进来
        default_queue: Box<dyn Qdisc<T, K>>, // 初始化时，传入平民底层队列
        global_bucket: GlobalBucket,
        high_bucket: HighBucket,
        reserved_bytes: usize,
        classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
    ) -> Self {
        Self {
            high_queue,
            default_queue,
            global_bucket,
            high_bucket,
            reserved_bytes,
            classifier,
        }
    }
}

// 实现 Qdisc 接口
impl<T, K, GlobalBucket, HighBucket>
    Qdisc<T, K>
    for RootHtbQdisc<T, K, GlobalBucket, HighBucket>
where
    GlobalBucket: TokenBucketLimiter,
    HighBucket: TokenBucketLimiter,
{
    fn enqueue(
        &mut self,
        ctx: PacketContext<T, K>,
    ) -> Result<(), PacketContext<T, K>> {
        if (self.classifier)(&ctx) {
            self.high_queue.enqueue(ctx)
        }else {
            self.default_queue.enqueue(ctx)
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 1. 特权队列判定 (✅ 现在统一调用 high_queue.peek())
        if let Some(ctx) = self.high_queue.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                return Some(ctx);
            }
        }

        // 2. 平民队列判定
        if let Some(ctx) = self.default_queue.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.reserved_bytes) {
                return Some(ctx);
            }
        }

        None
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 1. 特权出队逻辑
        if let Some(ctx) = self.high_queue.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                // ✅ 拆弹：安全提取。如果底层没吐出包，当无事发生，绝不 Panic
                if let Some(final_ctx) = self.high_queue.dequeue() {
                    self.high_bucket.consume(final_ctx.cost);
                    self.global_bucket.consume(final_ctx.cost);
                    return Some(final_ctx);
                }
            }
        }

        // 2. 平民出队逻辑
        if let Some(ctx) = self.default_queue.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.reserved_bytes) {
                // ✅ 拆弹：安全提取
                if let Some(final_ctx) = self.default_queue.dequeue() {
                    self.global_bucket.consume(final_ctx.cost);
                    return Some(final_ctx);
                }
            }
        }

        None
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut all_drops = Vec::new();
        all_drops.extend(self.high_queue.collect_dropped());
        all_drops.extend(self.default_queue.collect_dropped());
        all_drops
    }
}
