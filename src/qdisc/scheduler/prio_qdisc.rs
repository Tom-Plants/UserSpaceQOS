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
        // 🚦 岔路口：根据闭包的判断，把包物理分流
        if (self.classifier)(&ctx) {
            self.high_qdisc.enqueue(ctx)
        } else {
            self.low_qdisc.enqueue(ctx)
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 永远优先窥探高优队列
        if let Some(ctx) = self.high_qdisc.peek() {
            Some(ctx)
        } else {
            // 高优没包，才去看低优
            self.low_qdisc.peek()
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 核心特权魔法：阶级压制
        // 只要高优队列能掏出包，直接发走，直接 return！
        if let Some(ctx) = self.high_qdisc.dequeue() {
            return Some(ctx);
        }

        // 只有高优彻底空了（dequeue 返回 None），才轮到低优出局
        self.low_qdisc.dequeue()
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        // 收尸的时候一视同仁，把两边超时的死包都捞出来清掉
        let mut drops = self.high_qdisc.collect_dropped();
        drops.extend(self.low_qdisc.collect_dropped());
        drops
    }
}
