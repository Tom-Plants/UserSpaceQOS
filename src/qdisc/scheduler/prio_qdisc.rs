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

#[cfg(test)]
mod prio_qdisc_tests {
    use crate::qdisc::leaf::TTLHeadDropQdisc;

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 辅助工具：复用之前的全真队列
    // ==========================================

    fn make_pkt(msg: &'static str, is_high: bool) -> PacketContext<&'static str, bool> {
        PacketContext {
            msg,
            key: is_high,
            pkt_len: 100,
            cost: 100, // Prio 调度不看体积，只看阶级
            queue_num: 0,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 底层队列：保鲜期 1000ms，硬限设为 2 (为了测 Drop-Head)
    // 注意：假设你上下文中有一个 TTLHeadDropQdisc 的导入或定义
    fn make_real_qdisc() -> Box<dyn Qdisc<&'static str, bool>> {
        Box::new(TTLHeadDropQdisc::new(1000, 2))
    }

    // ==========================================
    // 阶级压制测试用例
    // ==========================================

    #[test]
    fn test_prio_basic_routing_and_strict_order() {
        // 分类器：key 为 true 则进高优
        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key);
        let mut prio = PrioQdisc::new(make_real_qdisc(), make_real_qdisc(), classifier);

        // 我们故意先塞低优，再塞高优
        prio.enqueue(make_pkt("Low-1", false));
        prio.enqueue(make_pkt("Low-2", false));

        // 高优大哥入场！直接插队！
        prio.enqueue(make_pkt("High-1", true));
        prio.enqueue(make_pkt("High-2", true));

        // 验证出队顺序：不管入队顺序如何，高优必须先出！
        assert_eq!(prio.dequeue().unwrap().msg, "High-1");
        assert_eq!(prio.dequeue().unwrap().msg, "High-2");

        // 高优彻底走完了，才轮到低优
        assert_eq!(prio.dequeue().unwrap().msg, "Low-1");
        assert_eq!(prio.dequeue().unwrap().msg, "Low-2");
        assert!(prio.dequeue().is_none());
    }

    #[test]
    fn test_prio_dynamic_suppression_and_peek() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key);
        let mut prio = PrioQdisc::new(make_real_qdisc(), make_real_qdisc(), classifier);

        // 场景：一开始只有低优包
        prio.enqueue(make_pkt("Low-1", false));

        // 此时 peek 看到的应该是低优包
        assert_eq!(prio.peek().unwrap().msg, "Low-1");

        // 突然，高优包进来了！
        prio.enqueue(make_pkt("High-1", true));

        // 验证 Peek 视角的动态切换：高优一来，Peek 的目光必须马上移向高优！
        assert_eq!(
            prio.peek().unwrap().msg,
            "High-1",
            "Peek 必须实时反映优先级"
        );

        // 高优出队
        assert_eq!(prio.dequeue().unwrap().msg, "High-1");

        // 高优走后，再次 Peek，低优才重新拥有姓名
        assert_eq!(prio.peek().unwrap().msg, "Low-1");
        assert_eq!(prio.dequeue().unwrap().msg, "Low-1");
    }

    #[test]
    fn test_prio_absolute_starvation() {
        // 测试极端的“饥饿”现象：只要高优源源不断地来，低优就永远出不去
        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key);
        let mut prio = PrioQdisc::new(make_real_qdisc(), make_real_qdisc(), classifier);

        prio.enqueue(make_pkt("Low-Poor-Guy", false));

        // 模拟高流量的高优突发
        prio.enqueue(make_pkt("High-1", true));
        assert_eq!(prio.dequeue().unwrap().msg, "High-1");

        // 就在低优以为轮到自己的时候，高优又来了
        prio.enqueue(make_pkt("High-2", true));
        assert_eq!(prio.dequeue().unwrap().msg, "High-2");

        prio.enqueue(make_pkt("High-3", true));
        assert_eq!(prio.dequeue().unwrap().msg, "High-3");

        // 确保在这期间，低优那个包一直被死死压在底下
        assert_eq!(prio.peek().unwrap().msg, "Low-Poor-Guy");
        assert_eq!(prio.dequeue().unwrap().msg, "Low-Poor-Guy");
    }

    #[test]
    fn test_prio_collect_dropped_bubbling() {
        // 验证垃圾回收的一视同仁
        let classifier = Box::new(|ctx: &PacketContext<&'static str, bool>| ctx.key);
        let mut prio = PrioQdisc::new(make_real_qdisc(), make_real_qdisc(), classifier);

        // 底层 real_qdisc 的容量硬限是 2，我们各塞 3 个包进去触发 Drop-Head
        prio.enqueue(make_pkt("High-Drop", true));
        prio.enqueue(make_pkt("High-Keep1", true));
        prio.enqueue(make_pkt("High-Keep2", true)); // 挤掉 High-Drop

        prio.enqueue(make_pkt("Low-Drop", false));
        prio.enqueue(make_pkt("Low-Keep1", false));
        prio.enqueue(make_pkt("Low-Keep2", false)); // 挤掉 Low-Drop

        // 倒垃圾时间！
        let drops = prio.collect_dropped();
        assert_eq!(drops.len(), 2, "应该收集到两边被挤掉的包");

        let drop_msgs: Vec<_> = drops.iter().map(|p| p.msg).collect();
        assert!(drop_msgs.contains(&"High-Drop"));
        assert!(drop_msgs.contains(&"Low-Drop"));
    }
}
