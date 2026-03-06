use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

// ==========================================
// 双通道公平轮询队列 (Dual Fair Qdisc)
// 保证两个子队列带宽 1:1 绝对公平，但内部逻辑互不干涉
// ==========================================
pub struct DualFairQdisc<T, K> {
    q_a: Box<dyn Qdisc<T, K>>,
    q_b: Box<dyn Qdisc<T, K>>,
    classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>, // true进A，false进B

    // DRR 公平账本
    deficit_a: i32,
    deficit_b: i32,
    quantum: i32, // 每次充值的配额 (通常设为 1500)
    turn_a: bool, // 记录当前是谁的回合 (true=A, false=B)
}

impl<T, K> DualFairQdisc<T, K> {
    pub fn new(
        q_a: Box<dyn Qdisc<T, K>>,
        q_b: Box<dyn Qdisc<T, K>>,
        quantum: i32,
        classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
    ) -> Self {
        Self {
            q_a,
            q_b,
            classifier,
            deficit_a: 0,
            deficit_b: 0,
            quantum,
            turn_a: true,
        }
    }
}

// 极其纯粹的入队出队实现
impl<T, K> Qdisc<T, K> for DualFairQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if (self.classifier)(&ctx) {
            self.q_a.enqueue(ctx)
        } else {
            self.q_b.enqueue(ctx)
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        loop {
            if self.q_a.peek().is_none() && self.q_b.peek().is_none() {
                self.deficit_a = 0;
                self.deficit_b = 0;
                return None;
            }

            if self.turn_a {
                if let Some(ctx) = self.q_a.peek() {
                    if self.deficit_a >= ctx.cost as i32 {
                        return self.q_a.peek(); // 定格！
                    }
                    self.deficit_a += self.quantum;
                } else {
                    self.deficit_a = 0;
                }
                self.turn_a = false; // 换 B
            } else {
                if let Some(ctx) = self.q_b.peek() {
                    if self.deficit_b >= ctx.cost as i32 {
                        return self.q_b.peek(); // 定格！
                    }
                    self.deficit_b += self.quantum;
                } else {
                    self.deficit_b = 0;
                }
                self.turn_a = true; // 换 A
            }
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 盲提货：peek 停在谁的回合，就扣谁的钱！
        if self.turn_a {
            let ctx = self.q_a.dequeue()?;
            self.deficit_a -= ctx.cost as i32;
            Some(ctx)
        } else {
            let ctx = self.q_b.dequeue()?;
            self.deficit_b -= ctx.cost as i32;
            Some(ctx)
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut drops = self.q_a.collect_dropped();
        drops.extend(self.q_b.collect_dropped());
        drops
    }
}
