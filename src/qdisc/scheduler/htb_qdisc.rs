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
    // 🚀 无返回值，直接塞
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if (self.classifier)(&ctx) {
            self.high_qdisc.enqueue(ctx);
        } else {
            self.low_qdisc.enqueue(ctx);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        let mut action: u8 = 0;

        // 🟢 阶段 1：保底带宽探测 (🚨 修改：花保底的钱，不需要给对方留准备金！)
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                action = 1;
            }
        }
        if action == 0 {
            if let Some(ctx) = self.low_qdisc.peek() {
                if self.low_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                    action = 2;
                }
            }
        }

        // 🟡 阶段 2：闲置带宽借用探测 (🚨 修改：借用公款，必须给对方留足准备金！)
        if action == 0 {
            if let Some(ctx) = self.high_qdisc.peek() {
                // 高优借钱，必须给低优留一口饭吃 (low_reserve)
                if self.global_bucket.can_spend(ctx.cost + self.low_reserve) {
                    action = 3;
                }
            }
        }
        if action == 0 {
            if let Some(ctx) = self.low_qdisc.peek() {
                // 低优借钱，必须给高优留足面子 (high_reserve)
                if self.global_bucket.can_spend(ctx.cost + self.high_reserve) {
                    action = 4;
                }
            }
        }

        match action {
            1 | 3 => self.high_qdisc.peek(),
            2 | 4 => self.low_qdisc.peek(),
            _ => None,
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🟢 阶段 1：花保底的钱
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.high_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                if let Some(real_ctx) = self.high_qdisc.dequeue() {
                    self.high_bucket.consume(real_ctx.cost);
                    self.global_bucket.consume(real_ctx.cost);
                    return Some(real_ctx);
                }
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.low_bucket.can_spend(ctx.cost) && self.global_bucket.can_spend(ctx.cost) {
                if let Some(real_ctx) = self.low_qdisc.dequeue() {
                    self.low_bucket.consume(real_ctx.cost);
                    self.global_bucket.consume(real_ctx.cost);
                    return Some(real_ctx);
                }
            }
        }

        // 🟡 阶段 2：花借用的钱
        if let Some(ctx) = self.high_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.low_reserve) {
                if let Some(real_ctx) = self.high_qdisc.dequeue() {
                    self.global_bucket.consume(real_ctx.cost);
                    return Some(real_ctx);
                }
            }
        }
        if let Some(ctx) = self.low_qdisc.peek() {
            if self.global_bucket.can_spend(ctx.cost + self.high_reserve) {
                if let Some(real_ctx) = self.low_qdisc.dequeue() {
                    self.global_bucket.consume(real_ctx.cost);
                    return Some(real_ctx);
                }
            }
        }

        None
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut drops = self.high_qdisc.collect_dropped();
        drops.extend(self.low_qdisc.collect_dropped());
        drops
    }
}

#[cfg(test)]
mod htb_tests {
    use crate::token_bucket::TokenBucket;

    use super::*;
    use std::collections::VecDeque;

    // ==========================================
    // 辅助工具：Mock 队列与 Mock 令牌桶
    // ==========================================

    fn make_pkt(
        msg: &'static str,
        is_high: bool,
        cost: usize,
    ) -> PacketContext<&'static str, bool> {
        PacketContext {
            msg,
            key: is_high,
            pkt_len: cost,
            cost,
            queue_num: 0,
            arrival_time: std::time::Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 一个极简的 FIFO 队列用于充当底层 Qdisc
    struct MockFifo<T, K> {
        queue: VecDeque<PacketContext<T, K>>,
    }
    impl<T, K> MockFifo<T, K> {
        fn new() -> Self {
            Self {
                queue: VecDeque::new(),
            }
        }
    }
    impl<T, K> Qdisc<T, K> for MockFifo<T, K> {
        fn enqueue(&mut self, ctx: PacketContext<T, K>) {
            self.queue.push_back(ctx);
        }
        fn peek(&mut self) -> Option<&PacketContext<T, K>> {
            self.queue.front()
        }
        fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
            self.queue.pop_front()
        }
    }

    // 🚀 精准控制余额的 Mock 令牌桶
    struct MockBucket {
        tokens: usize,
    }
    impl MockBucket {
        fn new(tokens: usize) -> Self {
            Self { tokens }
        }
    }
    impl TokenBucketLimiter for MockBucket {
        fn can_spend(&mut self, amount: usize) -> bool {
            self.tokens >= amount
        }
        fn consume(&mut self, amount: usize) -> bool {
            if self.tokens >= amount {
                self.tokens -= amount;
                true
            } else {
                false
            }
        }
    }

    // ==========================================
    // 核心逻辑测试用例
    // ==========================================

    #[test]
    fn test_htb_phase1_guaranteed_bandwidth() {
        // 配置：高低优桶都有钱，全局桶也是满的
        let high_bucket = MockBucket::new(100);
        let low_bucket = MockBucket::new(100);
        let global_bucket = MockBucket::new(1000); // 全局财大气粗

        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key); // true 为高优

        let mut htb = HtbQdisc::new(
            Box::new(MockFifo::new()),
            Box::new(MockFifo::new()),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0, // 无准备金
            classifier,
        );

        htb.enqueue(make_pkt("Low-1", false, 50));
        htb.enqueue(make_pkt("High-1", true, 50));

        // 验证 Phase 1 优先级：即使 Low 先入队，High 有保底额度，必须 High 先出！
        let p1 = htb.dequeue().unwrap();
        assert_eq!(p1.msg, "High-1");

        // High 出完后，Low 有保底额度，Low 出
        let p2 = htb.dequeue().unwrap();
        assert_eq!(p2.msg, "Low-1");

        assert!(htb.dequeue().is_none());
        // 验证扣款正确
        assert_eq!(htb.high_bucket.tokens, 50);
        assert_eq!(htb.low_bucket.tokens, 50);
        assert_eq!(htb.global_bucket.tokens, 900);
    }

    #[test]
    fn test_htb_phase2_borrowing() {
        // 配置：高低优专属桶**一分钱都没有**，只能靠借用全局桶
        let high_bucket = MockBucket::new(0);
        let low_bucket = MockBucket::new(0);
        let global_bucket = MockBucket::new(100);

        let mut htb = HtbQdisc::new(
            Box::new(MockFifo::new()),
            Box::new(MockFifo::new()),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0,
            Box::new(|ctx| ctx.key),
        );

        htb.enqueue(make_pkt("Low-1", false, 50));
        htb.enqueue(make_pkt("High-1", true, 50));

        // 验证 Phase 2 优先级：大家都没保底时，全局池借用也是 High 优先！
        assert_eq!(htb.dequeue().unwrap().msg, "High-1");
        assert_eq!(htb.dequeue().unwrap().msg, "Low-1");

        // 验证扣款：专属桶没动，全局桶扣光
        assert_eq!(htb.high_bucket.tokens, 0);
        assert_eq!(htb.low_bucket.tokens, 0);
        assert_eq!(htb.global_bucket.tokens, 0);
    }

    #[test]
    fn test_htb_reserve_protection() {
        // 🚀 核心看点：准备金机制能否防得住低优队列把带宽榨干？
        let high_bucket = MockBucket::new(0);
        let low_bucket = MockBucket::new(0);
        let global_bucket = MockBucket::new(100);

        // 设置 high_reserve = 60 (意味着低优来借钱时，全局池必须假装自己少了 60 块钱)
        let mut htb = HtbQdisc::new(
            Box::new(MockFifo::new()),
            Box::new(MockFifo::new()),
            high_bucket,
            low_bucket,
            global_bucket,
            60,
            0,
            Box::new(|ctx| ctx.key),
        );

        // Low 来借 50 块钱发包
        htb.enqueue(make_pkt("Low-Greedy", false, 50));

        // 验证：Low 队列 peek 借用时，需要 global.can_spend(50 + 60) -> 110。
        // 但是全局池只有 100，所以拒绝出队！
        assert!(
            htb.dequeue().is_none(),
            "触发准备金护航，低优包被拦截挂起！"
        );

        // 此时，高优包突然来了，体积 50
        htb.enqueue(make_pkt("High-VIP", true, 50));

        // 高优包 peek 借用时，需要 global.can_spend(50 + 0) -> 50。
        // 全局池有 100，成功发包！
        assert_eq!(htb.dequeue().unwrap().msg, "High-VIP");

        // 高优包发完后，全局池剩下 50。Low 继续被卡死。
        assert!(htb.dequeue().is_none());
    }

    #[test]
    fn test_htb_exhaustion_blocking() {
        // 配置：所有桶都没钱
        let high_bucket = MockBucket::new(0);
        let low_bucket = MockBucket::new(0);
        let global_bucket = MockBucket::new(0);

        let mut htb = HtbQdisc::new(
            Box::new(MockFifo::new()),
            Box::new(MockFifo::new()),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0,
            Box::new(|ctx| ctx.key),
        );

        htb.enqueue(make_pkt("High-1", true, 50));
        htb.enqueue(make_pkt("Low-1", false, 50));

        // 验证出队为空（产生阻塞，等待下一次时钟周期 Token 刷新）
        assert!(htb.dequeue().is_none());
        assert!(htb.peek().is_none());
    }

    #[test]
    fn test_real_token_bucket_math() {
        // 顺手测一下你写的真实的 TokenBucket 数学计算
        // 速率: 1000 bytes/sec, 容量: 500 bytes
        let mut real_bucket = TokenBucket::new(1000.0, 500.0, "Test");

        // 刚初始化时，桶是满的 (突发容量)
        assert!(real_bucket.can_spend(500));
        assert!(real_bucket.consume(500));

        // 消耗完后，马上请求会失败
        assert!(!real_bucket.can_spend(100));

        // 休眠 150 毫秒，按照 1000 bytes/sec，应该会恢复大约 150 bytes
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(real_bucket.can_spend(100), "休眠后令牌应该得到了补充");
        assert!(real_bucket.consume(100));
    }
}
#[cfg(test)]
mod real_htb_tests {
    use crate::{qdisc::leaf::TTLHeadDropQdisc, token_bucket::TokenBucket};

    use super::*;
    use std::time::{Duration, Instant};

    // ==========================================
    // 全真环境辅助工具
    // ==========================================

    fn make_pkt(
        msg: &'static str,
        is_high: bool,
        cost: usize,
    ) -> PacketContext<&'static str, bool> {
        PacketContext {
            msg,
            key: is_high,
            pkt_len: cost,
            cost,
            queue_num: 0,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 制造真实的底层队列：保鲜期 1000ms，容量上限 2 (用于测试 Drop-Head)
    fn make_real_qdisc() -> Box<dyn Qdisc<&'static str, bool>> {
        Box::new(TTLHeadDropQdisc::new(1000, 2))
    }

    // ==========================================
    // 全真集成测试用例
    // ==========================================

    #[test]
    fn test_real_htb_phase1_guaranteed_bandwidth() {
        // 初始给足突发容量 (Burst)，假装桶里满满的都是钱
        let high_bucket = TokenBucket::new(1000.0, 1000.0, "High");
        let low_bucket = TokenBucket::new(1000.0, 1000.0, "Low");
        let global_bucket = TokenBucket::new(10000.0, 10000.0, "Global");

        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key); // true 为高优

        let mut htb = HtbQdisc::new(
            make_real_qdisc(),
            make_real_qdisc(),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0, // 无准备金
            classifier,
        );

        htb.enqueue(make_pkt("Low-1", false, 50));
        htb.enqueue(make_pkt("High-1", true, 50));

        // 验证 Phase 1：High 和 Low 都在底层队列里，且都有专属保底额度。
        // HTB 的 peek 会优先检查 High 队列，所以必须是 High 先出！
        assert_eq!(htb.dequeue().unwrap().msg, "High-1");
        assert_eq!(htb.dequeue().unwrap().msg, "Low-1");
        assert!(htb.dequeue().is_none());

        // 验证真实桶的扣费逻辑
        assert!(htb.high_bucket.tokens <= 950.0); // 浮点数可能因为瞬间时间流逝有一点点回血
        assert!(htb.low_bucket.tokens <= 950.0);
        assert!(htb.global_bucket.tokens <= 9900.0); // 两个包各扣 50
    }

    #[test]
    fn test_real_htb_phase2_borrowing() {
        // 配置：高低优专属桶**容量为 0**，一出生就是穷光蛋，只能靠借用全局桶
        let high_bucket = TokenBucket::new(0.0, 0.0, "High");
        let low_bucket = TokenBucket::new(0.0, 0.0, "Low");
        let global_bucket = TokenBucket::new(1000.0, 1000.0, "Global");

        let mut htb = HtbQdisc::new(
            make_real_qdisc(),
            make_real_qdisc(),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0,
            Box::new(|ctx| ctx.key),
        );

        htb.enqueue(make_pkt("Low-1", false, 50));
        htb.enqueue(make_pkt("High-1", true, 50));

        // 阶段 1 (保底) 全部失败，进入阶段 2 (借用)。
        // 借用阶段依然严格遵守 High 优先！
        assert_eq!(htb.dequeue().unwrap().msg, "High-1");
        assert_eq!(htb.dequeue().unwrap().msg, "Low-1");
    }

    #[test]
    fn test_real_htb_reserve_protection() {
        // 核心验证：真实的 TokenBucket 结合 reserve (准备金) 机制
        let high_bucket = TokenBucket::new(0.0, 0.0, "High");
        let low_bucket = TokenBucket::new(0.0, 0.0, "Low");

        // 全局池里只有 100 块钱
        let global_bucket = TokenBucket::new(100.0, 100.0, "Global");

        let mut htb = HtbQdisc::new(
            make_real_qdisc(),
            make_real_qdisc(),
            high_bucket,
            low_bucket,
            global_bucket,
            60,
            0, // 🚀 高优准备金 60
            Box::new(|ctx| ctx.key),
        );

        // Low 来借 50 块钱
        htb.enqueue(make_pkt("Low-Greedy", false, 50));

        // Low 借款条件：global >= 50 + 60(准备金) = 110。
        // 但 global 只有 100，所以出不来！
        assert!(htb.dequeue().is_none());

        // High 突然杀到，也是 50 块钱
        htb.enqueue(make_pkt("High-VIP", true, 50));

        // High 借款条件：global >= 50 + 0 = 50。
        // global 有 100，直接放行！
        assert_eq!(htb.dequeue().unwrap().msg, "High-VIP");

        // High 拿走 50 后，全局剩 50。Low 依然被死死卡住。
        assert!(htb.dequeue().is_none());
    }

    #[test]
    fn test_real_htb_underlying_drop_bubbling() {
        // 验证 HTB 和底层的 TTLHeadDropQdisc 联动！
        // 我们给足网速，防止限速器卡住包
        let high_bucket = TokenBucket::new(10000.0, 10000.0, "High");
        let low_bucket = TokenBucket::new(10000.0, 10000.0, "Low");
        let global_bucket = TokenBucket::new(10000.0, 10000.0, "Global");

        let mut htb = HtbQdisc::new(
            make_real_qdisc(), // 内部 hard_limit 是 2
            make_real_qdisc(), // 内部 hard_limit 是 2
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0,
            Box::new(|ctx| ctx.key),
        );

        // 强行往 High 队列塞入 3 个包，由于底层容量为 2，第一个会被 Drop-Head 踢掉！
        htb.enqueue(make_pkt("High-DropMe", true, 50));
        htb.enqueue(make_pkt("High-Keep1", true, 50));
        htb.enqueue(make_pkt("High-Keep2", true, 50));

        // 倒垃圾时间！
        let drops = htb.collect_dropped();
        assert_eq!(drops.len(), 1, "应该精准摸到底层被超载丢弃的那个包");
        assert_eq!(drops[0].msg, "High-DropMe");

        // 正常出队的必须是保留下来的两个包
        assert_eq!(htb.dequeue().unwrap().msg, "High-Keep1");
        assert_eq!(htb.dequeue().unwrap().msg, "High-Keep2");
    }

    #[test]
    fn test_real_htb_time_refill() {
        // 1. 容量给够 (1000)，初始金额也是 1000
        let mut high_bucket = TokenBucket::new(1000.0, 1000.0, "High");
        let low_bucket = TokenBucket::new(0.0, 0.0, "Low");
        let mut global_bucket = TokenBucket::new(1000.0, 1000.0, "Global");

        // 🚀 2. 手动提款！把它们的余额抽干到 500
        high_bucket.consume(500);
        global_bucket.consume(500);

        let mut htb = HtbQdisc::new(
            make_real_qdisc(),
            make_real_qdisc(),
            high_bucket,
            low_bucket,
            global_bucket,
            0,
            0,
            Box::new(|ctx| ctx.key),
        );

        // 3. 塞入大包 (Cost = 600)。此时桶里只有 500 块钱，不够发。
        htb.enqueue(make_pkt("Big-High", true, 600));

        // 第一次尝试：钱不够，被限速器无情卡住
        assert!(htb.dequeue().is_none());

        // 4. 让线程睡 150 毫秒。按 1000 bytes/s 的速率，大约能回复 150 块钱。
        // 此时 500 + 150 = 650。因为 capacity 是 1000，所以它能存得下这笔钱！
        std::thread::sleep(Duration::from_millis(150));

        // 5. 第二次尝试：触发 can_spend 里的 refill()，发现余额变成 650 了，成功发包！
        let out = htb.dequeue();
        assert!(out.is_some(), "休眠回血后，巨型包应该成功出队");
        assert_eq!(out.unwrap().msg, "Big-High");
    }
}
