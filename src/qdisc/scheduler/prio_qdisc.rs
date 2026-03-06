use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

// ==========================================
// 严格优先级调度器 (Strict Priority Qdisc)
// 绝对的阶级压制：只要高优有包，低优绝对不发
// ==========================================
pub struct PrioQdisc<T, K> {
    high_qdisc: Box<dyn Qdisc<T, K>>,
    low_qdisc: Box<dyn Qdisc<T, K>>,

    // 🚀 核心：闭包分类器
    // 接收包的上下文面单，返回 true 进高优，返回 false 进低优
    classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
}

impl<T, K> PrioQdisc<T, K> {
    pub fn new(
        high_qdisc: Box<dyn Qdisc<T, K>>,
        low_qdisc: Box<dyn Qdisc<T, K>>,
        classifier: Box<dyn Fn(&PacketContext<T, K>) -> bool>,
    ) -> Self {
        Self {
            high_qdisc,
            low_qdisc,
            classifier,
        }
    }
}

// ==========================================
// 实现统一的 Qdisc 接口
// ==========================================
impl<T, K> Qdisc<T, K> for PrioQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if (self.classifier)(&ctx) {
            self.high_qdisc.enqueue(ctx);
        } else {
            self.low_qdisc.enqueue(ctx);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 高优看得到，就锁死高优；看不到才轮到低优
        if self.high_qdisc.peek().is_some() {
            self.high_qdisc.peek()
        } else {
            self.low_qdisc.peek()
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 盲提货：peek 看的是谁，就提谁！
        if self.high_qdisc.peek().is_some() {
            self.high_qdisc.dequeue()
        } else {
            self.low_qdisc.dequeue()
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek();
        let mut drops = self.high_qdisc.collect_dropped();
        drops.extend(self.low_qdisc.collect_dropped());
        drops
    }
}
