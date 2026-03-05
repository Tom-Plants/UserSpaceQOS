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

    /// 核心状态机：寻找下一个有钱且有包的队列
    fn prepare_next(&mut self) -> bool {
        loop {
            // 🚀 核心修复：每次循环都查一次，防止底层偷偷把包扔了导致两边全空！
            if self.q_a.peek().is_none() && self.q_b.peek().is_none() {
                self.deficit_a = 0;
                self.deficit_b = 0;
                return false;
            }

            if self.turn_a {
                if let Some(ctx) = self.q_a.peek() {
                    if self.deficit_a >= ctx.cost as i32 {
                        return true;
                    }
                    self.deficit_a += self.quantum;
                } else {
                    self.deficit_a = 0;
                }
                self.turn_a = false;
            } else {
                if let Some(ctx) = self.q_b.peek() {
                    if self.deficit_b >= ctx.cost as i32 {
                        return true;
                    }
                    self.deficit_b += self.quantum;
                } else {
                    self.deficit_b = 0;
                }
                self.turn_a = true;
            }
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
        if !self.prepare_next() {
            return None;
        }
        if self.turn_a {
            self.q_a.peek()
        } else {
            self.q_b.peek()
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        if !self.prepare_next() {
            return None;
        }

        // 提货并扣款！按净重/物理体积 (cost) 扣费，保证绝对的字节公平
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

#[cfg(test)]
mod tests {
    use crate::qdisc::leaf::TTLHeadDropQdisc;

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 辅助工具
    // ==========================================

    fn make_pkt(msg: &'static str, key: u32, cost: usize) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key,
            pkt_len: cost,
            cost,
            queue_num: 0,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 辅助工厂：生成带 TTL 和容量限制的底层队列
    fn make_sub_queue() -> Box<dyn Qdisc<&'static str, u32>> {
        Box::new(TTLHeadDropQdisc::new(100, 10))
    }

    // ==========================================
    // 测试用例
    // ==========================================

    #[test]
    fn test_dual_fair_basic_routing() {
        // 分类器：key 为 1 进 A，否则进 B
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        let mut qdisc = DualFairQdisc::new(make_sub_queue(), make_sub_queue(), 100, classifier);

        qdisc.enqueue(make_pkt("To-A", 1, 50));
        qdisc.enqueue(make_pkt("To-B", 2, 50));

        // 因为初始 turn_a = true，并且 A 的 deficit 攒到 100 > 50，所以先出 A
        assert_eq!(qdisc.dequeue().unwrap().msg, "To-A");
        // A 空了，切换到 B
        assert_eq!(qdisc.dequeue().unwrap().msg, "To-B");
        assert!(qdisc.dequeue().is_none());
    }

    #[test]
    fn test_dual_fair_alternating() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        // quantum 为 100
        let mut qdisc = DualFairQdisc::new(make_sub_queue(), make_sub_queue(), 100, classifier);

        // A 和 B 各入队两包，cost 都是 100
        qdisc.enqueue(make_pkt("A1", 1, 100));
        qdisc.enqueue(make_pkt("A2", 1, 100));
        qdisc.enqueue(make_pkt("B1", 2, 100));
        qdisc.enqueue(make_pkt("B2", 2, 100));

        // 验证 1:1 绝对交替 (因为每个包刚好消耗完 quantum)
        // 过程: A 攒到100 -> 发A1 -> deficit_a=0 -> A攒100(不够继续发) -> 轮到B -> B攒100 -> 发B1 ...
        assert_eq!(qdisc.dequeue().unwrap().msg, "A1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "A2");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B2");
        assert!(qdisc.dequeue().is_none());
    }

    #[test]
    fn test_dual_fair_bursting() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        // 🚀 quantum 设为 300，意味着每个队列一次回合最多能发 300 cost 的包
        let mut qdisc = DualFairQdisc::new(make_sub_queue(), make_sub_queue(), 300, classifier);

        // A 入队 3 个小包 (cost 100)
        qdisc.enqueue(make_pkt("A1", 1, 100));
        qdisc.enqueue(make_pkt("A2", 1, 100));
        qdisc.enqueue(make_pkt("A3", 1, 100));
        // B 入队 3 个小包
        qdisc.enqueue(make_pkt("B1", 2, 100));
        qdisc.enqueue(make_pkt("B2", 2, 100));
        qdisc.enqueue(make_pkt("B3", 2, 100));

        // A 拿到 300 额度，应该一口气连发 3 个，而不是交替！
        assert_eq!(qdisc.dequeue().unwrap().msg, "A1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "A2");
        assert_eq!(qdisc.dequeue().unwrap().msg, "A3");
        // A 发完了（或额度耗尽了），轮到 B 连发
        assert_eq!(qdisc.dequeue().unwrap().msg, "B1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B2");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B3");
    }

    #[test]
    fn test_dual_fair_deficit_accumulation() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        // quantum 为 100
        let mut qdisc = DualFairQdisc::new(make_sub_queue(), make_sub_queue(), 100, classifier);

        // A 是巨型包(cost 250)，B 是普通包(cost 100)
        qdisc.enqueue(make_pkt("A-Jumbo", 1, 250));
        qdisc.enqueue(make_pkt("B1", 2, 100));
        qdisc.enqueue(make_pkt("B2", 2, 100));
        qdisc.enqueue(make_pkt("B3", 2, 100));

        // 极硬核的过程推演：
        // 1. A_def += 100 (100<250，切B)
        // 2. B_def += 100 (100>=100，出B1，B_def=0，依然是B的回合)
        // 3. peek检查B，def=0<100，B_def+=100，切A
        // 4. A_def += 100 (200<250，切B)
        // 5. B_def (刚才攒了100) >= 100，出B2，B_def=0
        // 6. ...
        // 结论：B 会先出两个包，A 攒够 300 才能发出 Jumbo！
        assert_eq!(qdisc.dequeue().unwrap().msg, "B1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B2");
        assert_eq!(qdisc.dequeue().unwrap().msg, "A-Jumbo"); // A 终于攒够钱了！
        assert_eq!(qdisc.dequeue().unwrap().msg, "B3");
    }

    #[test]
    fn test_dual_fair_collect_dropped() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        // 底层队列 hard_limit 为 2，超出就丢
        let mut q_a = Box::new(TTLHeadDropQdisc::new(100, 2));
        let mut q_b = Box::new(TTLHeadDropQdisc::new(100, 2));

        // 我们需要手动塞爆它们来验证垃圾回收
        q_a.enqueue(make_pkt("A-Drop", 1, 50));
        q_a.enqueue(make_pkt("A-Keep1", 1, 50));
        q_a.enqueue(make_pkt("A-Keep2", 1, 50)); // A-Drop 被踢出

        q_b.enqueue(make_pkt("B-Drop", 2, 50));
        q_b.enqueue(make_pkt("B-Keep1", 2, 50));
        q_b.enqueue(make_pkt("B-Keep2", 2, 50)); // B-Drop 被踢出

        let mut qdisc = DualFairQdisc::new(q_a, q_b, 100, classifier);

        let drops = qdisc.collect_dropped();
        assert_eq!(drops.len(), 2, "应该收集到从 A 和 B 踢出来的各 1 个包");

        // 验证丢弃的是旧包
        let drop_msgs: Vec<_> = drops.iter().map(|p| p.msg).collect();
        assert!(drop_msgs.contains(&"A-Drop"));
        assert!(drop_msgs.contains(&"B-Drop"));
    }

    #[test]
    fn test_dual_fair_exhaustion_reset() {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        let mut qdisc = DualFairQdisc::new(make_sub_queue(), make_sub_queue(), 100, classifier);

        // 只给 B 发包，A 始终为空
        qdisc.enqueue(make_pkt("B1", 2, 100));
        qdisc.enqueue(make_pkt("B2", 2, 100));

        assert_eq!(qdisc.dequeue().unwrap().msg, "B1");
        assert_eq!(qdisc.dequeue().unwrap().msg, "B2");

        // 全部掏空后，验证 reset 逻辑生效
        assert!(qdisc.peek().is_none());
        assert_eq!(qdisc.deficit_a, 0);
        assert_eq!(qdisc.deficit_b, 0);
    }
}

#[cfg(test)]
mod nested_dual_fair_tests {
    use crate::qdisc::{leaf::TTLHeadDropQdisc, scheduler::ClassDrrQdisc};

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 三层嵌套辅助工具
    // Root: DualFair -> Child: ClassDrr -> Leaf: TTLHeadDrop
    // ==========================================

    // dual_key 决定进 DualFair 的 A(1) 还是 B(2)
    // drr_class 决定进 ClassDrr 的哪个子队列
    fn make_pkt(
        msg: &'static str,
        dual_key: u32,
        drr_class: usize,
        cost: usize,
    ) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key: dual_key,
            pkt_len: cost,
            cost,
            queue_num: drr_class,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 底层叶子：hard_limit 设为 3 方便测试踢人
    fn leaf_factory() -> Box<dyn Fn() -> Box<dyn Qdisc<&'static str, u32>>> {
        Box::new(|| Box::new(TTLHeadDropQdisc::new(1000, 3)))
    }

    // 中间层：根据 queue_num 分类的 ClassDrr
    fn make_drr_branch() -> Box<dyn Qdisc<&'static str, u32>> {
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.queue_num, 100));
        Box::new(ClassDrrQdisc::new(classifier, leaf_factory()))
    }

    // ==========================================
    // 嵌套测试用例
    // ==========================================

    #[test]
    fn test_nested_dual_fair_basic_routing() {
        // 顶层 DualFair：key == 1 进 A 分支，否则进 B 分支
        let top_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        let mut root =
            DualFairQdisc::new(make_drr_branch(), make_drr_branch(), 100, top_classifier);

        // A 分支 (key=1), Class 10
        root.enqueue(make_pkt("A-C10", 1, 10, 100));
        // B 分支 (key=2), Class 20
        root.enqueue(make_pkt("B-C20", 2, 20, 100));

        // 验证交替出包
        assert_eq!(root.dequeue().unwrap().msg, "A-C10");
        assert_eq!(root.dequeue().unwrap().msg, "B-C20");
        assert!(root.dequeue().is_none());
    }

    #[test]
    fn test_nested_dual_fair_deep_jumbo_frame() {
        let top_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        let mut root =
            DualFairQdisc::new(make_drr_branch(), make_drr_branch(), 100, top_classifier);

        // 极端场景：A 分支的子队列 10 收到一个巨型包 (cost 250)
        root.enqueue(make_pkt("A-Jumbo", 1, 10, 250));

        // B 分支的子队列 20 收到 3 个普通包 (cost 100)
        root.enqueue(make_pkt("B-Normal1", 2, 20, 100));
        root.enqueue(make_pkt("B-Normal2", 2, 20, 100));
        root.enqueue(make_pkt("B-Normal3", 2, 20, 100));

        // 精彩推演：
        // 轮次1：A (Deficit 100 < 250) 无法出包；B (Deficit 100 >= 100) 正常出包
        assert_eq!(root.dequeue().unwrap().msg, "B-Normal1");

        // 轮次2：A (Deficit 200 < 250) 依然卡住；B (Deficit 100 >= 100) 正常出包
        assert_eq!(root.dequeue().unwrap().msg, "B-Normal2");

        // 轮次3：A (Deficit 300 >= 250) 终于攒够钱了！双重调度器放行巨型包！
        assert_eq!(root.dequeue().unwrap().msg, "A-Jumbo");

        // 轮次4：B 剩下的小包正常出
        assert_eq!(root.dequeue().unwrap().msg, "B-Normal3");
    }

    #[test]
    fn test_nested_dual_fair_deep_drop_bubbling() {
        let top_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| ctx.key == 1);
        let mut root =
            DualFairQdisc::new(make_drr_branch(), make_drr_branch(), 100, top_classifier);

        // 叶子节点的 hard_limit = 3。
        // 我们往 [A分支 -> 子队列10] 里强塞 4 个包，触发最底层的 Drop-Head
        root.enqueue(make_pkt("DropMe-A", 1, 10, 50));
        root.enqueue(make_pkt("Keep1-A", 1, 10, 50));
        root.enqueue(make_pkt("Keep2-A", 1, 10, 50));
        root.enqueue(make_pkt("Keep3-A", 1, 10, 50)); // "DropMe-A" 被物理挤出去了

        // 再往 [B分支 -> 子队列20] 里也强塞 4 个包
        root.enqueue(make_pkt("DropMe-B", 2, 20, 50));
        root.enqueue(make_pkt("Keep1-B", 2, 20, 50));
        root.enqueue(make_pkt("Keep2-B", 2, 20, 50));
        root.enqueue(make_pkt("Keep3-B", 2, 20, 50)); // "DropMe-B" 被物理挤出去了

        // 见证奇迹的时刻：在最顶层（DualFair）倒垃圾！
        // 垃圾路线：TTLHeadDrop -> ClassDrr -> DualFair
        let drops = root.collect_dropped();

        assert_eq!(drops.len(), 2, "应该精准捞出 2 个掉在最底层的包");

        let drop_msgs: Vec<_> = drops.iter().map(|p| p.msg).collect();
        assert!(drop_msgs.contains(&"DropMe-A"));
        assert!(drop_msgs.contains(&"DropMe-B"));

        // 验证出包逻辑没有因为底层的丢弃而崩溃
        // 验证出包逻辑：因为 DualFair 的 quantum 是 100，而每个包是 50。
        // 所以必然是 A 连发两个，再轮到 B 连发两个！
        assert_eq!(root.dequeue().unwrap().msg, "Keep1-A"); // A 花费 50，剩余 50
        assert_eq!(root.dequeue().unwrap().msg, "Keep2-A"); // A 花费 50，剩余 0，回合结束

        assert_eq!(root.dequeue().unwrap().msg, "Keep1-B"); // B 拿到 100，花费 50，剩余 50
        assert_eq!(root.dequeue().unwrap().msg, "Keep2-B"); // B 花费 50，剩余 0，回合结束

        assert_eq!(root.dequeue().unwrap().msg, "Keep3-A"); // A 再次拿到 100，发包
        assert_eq!(root.dequeue().unwrap().msg, "Keep3-B"); // B 再次拿到 100，发包

        // 全部掏空
        assert!(root.dequeue().is_none());
    }
}
