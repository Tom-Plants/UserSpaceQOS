use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

// ==========================================
// 终极大类调度器：ClassDrrQdisc (纯粹的带权轮询分发器)
// ==========================================

// 内部缓冲区：现在装的是纯泛型 Inner
struct ClassBuffer<T, K> {
    inner_qdisc: Box<dyn Qdisc<T, K>>, // ✅ 彻底泛型化，它可以是任何实现了 Qdisc 的东西！
    deficit: i32,
    quantum: i32,
}

pub struct ClassDrrQdisc<T, K, C> {
    classes: HashMap<C, ClassBuffer<T, K>>,
    active_classes: VecDeque<C>,
    // 🚀 注入的分类器：接收面单，告诉你它属于哪个 class_id，以及量子配额是多少
    classifier: Box<dyn Fn(&PacketContext<T, K>) -> (C, i32)>,

    // 🚀 注入的兵工厂：当发现新的 class_id 时，动态制造底层队列
    inner_factory: Box<dyn Fn() -> Box<dyn Qdisc<T, K>>>,
    pending_drops: Vec<PacketContext<T, K>>,
}

impl<T, K, C> ClassDrrQdisc<T, K, C>
where
    K: Hash + Eq + Clone,
    C: Hash + Eq + Clone,
{
    pub fn new(
        // 🚀 注入的分类器：接收面单，告诉你它属于哪个 class_id，以及量子配额是多少
        classifier: Box<dyn Fn(&PacketContext<T, K>) -> (C, i32)>,

        // 🚀 注入的兵工厂：当发现新的 class_id 时，动态制造底层队列
        inner_factory: Box<dyn Fn() -> Box<dyn Qdisc<T, K>>>,
    ) -> Self {
        Self {
            classes: HashMap::new(),
            active_classes: VecDeque::new(),
            classifier,
            inner_factory,
            pending_drops: Vec::new(),
        }
    }

    // 状态机：寻找下一个有钱发包的大类
    fn prepare_next_ready_class(&mut self) -> bool {
        loop {
            // ... 前面获取 class_id 保持不变
            let class_id = match self.active_classes.pop_front() {
                Some(id) => id,
                None => return false,
            };

            let mut remove_class = false;
            let mut move_to_back = false;

            if let Some(class) = self.classes.get_mut(&class_id) {
                if let Some(ctx) = class.inner_qdisc.peek() {
                    let len = ctx.cost as i32;
                    if class.deficit < len {
                        class.deficit += class.quantum;
                        move_to_back = true;
                    } else {
                        self.active_classes.push_front(class_id);
                        return true;
                    }
                } else {
                    remove_class = true;
                }

                // 🚀 核心：无论它是否要被销毁，都把它的垃圾桶掏空！
                self.pending_drops
                    .extend(class.inner_qdisc.collect_dropped());
            } else {
                continue;
            }

            // 存入大类的收容所
            if remove_class {
                self.classes.remove(&class_id);
            } else if move_to_back {
                self.active_classes.push_back(class_id);
            }
        }
    }
}

// ==========================================
// 实现统一的 Qdisc 接口
// 入场券现在是: (大类ID, 大类Quantum, 传给底层的Param)
// ==========================================
impl<T, K, C> Qdisc<T, K> for ClassDrrQdisc<T, K, C>
where
    K: Hash + Eq + Clone,
    C: Hash + Eq + Clone,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> () {
        let (class_id, class_quantum) = (self.classifier)(&ctx);

        let class = match self.classes.entry(class_id.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(ClassBuffer {
                inner_qdisc: (self.inner_factory)(),
                deficit: class_quantum,
                quantum: class_quantum,
            }),
        };

        class.quantum = class_quantum;
        class.inner_qdisc.enqueue(ctx);

        // 🚀 核心终极修复：绝对幂等激活！
        // 不再依赖难以捉摸的 peek() 瞬间状态，而是直接检查轮询名单里有没有它。
        // 由于 active_classes 通常很短（活跃流数量），这里的 contains 检查开销微乎其微，
        // 却能 100% 根绝“残影双黄蛋”导致的静态死锁问题！
        if !self.active_classes.contains(&class_id) {
            self.active_classes.push_front(class_id);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if !self.prepare_next_ready_class() {
            return None;
        }
        let class_id = self.active_classes.front()?;
        self.classes.get_mut(class_id)?.inner_qdisc.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        loop {
            if !self.prepare_next_ready_class() {
                return None;
            }

            let class_id = self.active_classes.pop_front()?;

            let (ctx_opt, is_empty, rescued_drops) = {
                // 安全获取，忽略残影
                let class = match self.classes.get_mut(&class_id) {
                    Some(c) => c,
                    None => continue,
                };

                let ctx_opt = class.inner_qdisc.dequeue();

                // 🚨 核心修复：把遗失的“结账扣款”逻辑加回来！
                if let Some(ctx) = &ctx_opt {
                    class.deficit -= ctx.cost as i32;
                }

                let empty = class.inner_qdisc.peek().is_none();
                let drops = class.inner_qdisc.collect_dropped();

                (ctx_opt, empty, drops)
            };

            // 存入收容所
            self.pending_drops.extend(rescued_drops);

            if is_empty {
                self.classes.remove(&class_id);
            } else {
                self.active_classes.push_front(class_id);
            }

            if let Some(ctx) = ctx_opt {
                return Some(ctx);
            }
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        // 🚀 1. 把之前摸尸攒下来的存货拿出来
        let mut all_drops = std::mem::take(&mut self.pending_drops);

        // 🚀 2. 顺便搜刮一下当前还活着的子队列
        for class in self.classes.values_mut() {
            all_drops.extend(class.inner_qdisc.collect_dropped());
        }
        all_drops
    }
}

#[cfg(test)]
mod tests {
    use crate::qdisc::leaf::TTLHeadDropQdisc;

    use super::*;
    use std::time::{Duration, Instant};

    // ==========================================
    // 辅助工具：兵工厂与发包器
    // ==========================================

    // 辅助函数：快速构造 PacketContext
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

    // 辅助函数：构造一个“过去某时刻”到达的包，用于测试 TTL 超时
    fn make_aged_pkt(
        msg: &'static str,
        key: u32,
        cost: usize,
        aged_ms: u64,
    ) -> PacketContext<&'static str, u32> {
        let mut pkt = make_pkt(msg, key, cost);
        // 手动把包的到达时间往前拨
        pkt.arrival_time = Instant::now() - Duration::from_millis(aged_ms);
        pkt
    }

    // 辅助工厂：生成带 TTL 和硬性容量限制的底层队列
    // 参数配置：保鲜期 100ms，容量上限 3 个包
    fn real_qdisc_factory() -> Box<dyn Fn() -> Box<dyn Qdisc<&'static str, u32>>> {
        Box::new(|| Box::new(TTLHeadDropQdisc::new(100, 3)))
    }

    // ==========================================
    // 测试用例
    // ==========================================

    #[test]
    fn test_drr_basic_enqueue_dequeue() {
        // 分类器：根据 key (flow_id) 分配 class，配额统一定为 100
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut drr = ClassDrrQdisc::new(classifier, real_qdisc_factory());

        assert!(drr.peek().is_none());

        drr.enqueue(make_pkt("A1", 1, 50));

        // 验证 Peek 不会消耗包
        assert_eq!(drr.peek().unwrap().msg, "A1");

        // 验证 Dequeue 正常出包
        let pkt = drr.dequeue().unwrap();
        assert_eq!(pkt.msg, "A1");
        assert!(drr.dequeue().is_none()); // 掏空后应为空
    }

    #[test]
    fn test_drr_fair_round_robin() {
        // 分类器：(key, quantum=100)
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut drr = ClassDrrQdisc::new(classifier, real_qdisc_factory());

        // 两个流各入队两个包
        drr.enqueue(make_pkt("Flow1-A", 1, 100));
        drr.enqueue(make_pkt("Flow1-B", 1, 100));
        drr.enqueue(make_pkt("Flow2-A", 2, 100));
        drr.enqueue(make_pkt("Flow2-B", 2, 100));

        // 验证交替出队 (具体谁先出取决于哈希/入队顺序，但一定是 1-2-1-2 或 2-1-2-1)
        let first = drr.dequeue().unwrap();
        let second = drr.dequeue().unwrap();
        let third = drr.dequeue().unwrap();
        let fourth = drr.dequeue().unwrap();

        // 它们不应该连续出同一个流的包
        assert_ne!(first.key, second.key);
        assert_ne!(second.key, third.key);
        assert_eq!(first.key, third.key);
        assert_eq!(second.key, fourth.key);
    }

    #[test]
    fn test_drr_deficit_accumulation_for_jumbo_frames() {
        // Flow 1 的配额只有 50，但它发了一个 Cost 为 150 的巨型包
        // Flow 2 的配额是 100，发的是 Cost 100 的正常包
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| match ctx.key {
            1 => (ctx.key, 50),
            2 => (ctx.key, 100),
            3 => (ctx.key, 50),
            _ => panic!(),
        });
        let mut drr = ClassDrrQdisc::new(classifier, real_qdisc_factory());

        drr.enqueue(make_pkt("Jumbo-1", 1, 150));
        drr.enqueue(make_pkt("Normal-1", 3, 50));
        drr.enqueue(make_pkt("Normal-2", 2, 100));

        // 第一轮：Flow 2 配额够，直接出；Flow 1 积攒赤字 (deficit: 50 -> 100)
        let pkt1 = drr.dequeue().unwrap();
        assert_eq!(pkt1.msg, "Normal-2");

        let pkt2 = drr.dequeue().unwrap();
        assert_eq!(pkt2.msg, "Normal-1");

        // 第二轮：Flow 1 积攒赤字 (deficit: 100 -> 150)，终于够钱出队了！
        let pkt3 = drr.dequeue().unwrap();
        assert_eq!(pkt3.msg, "Jumbo-1");
    }

    #[test]
    fn test_integration_ttl_hard_limit_drop_head() {
        // 使用 hard_limit = 3 的底层队列
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut drr = ClassDrrQdisc::new(classifier, real_qdisc_factory());

        // 强行塞入 4 个包到同一个队列，触发 Drop-Head (踢掉最老的)
        drr.enqueue(make_pkt("P1", 1, 50)); // 这个会被踢掉！
        drr.enqueue(make_pkt("P2", 1, 50));
        drr.enqueue(make_pkt("P3", 1, 50));
        drr.enqueue(make_pkt("P4", 1, 50));

        // 从大类收集垃圾
        let drops = drr.collect_dropped();
        assert_eq!(drops.len(), 1);
        assert_eq!(
            drops[0].msg, "P1",
            "由于超载，队头的 P1 应该被 Drop-Head 机制踢掉"
        );

        // 正常出队的应该是 P2, P3, P4
        assert_eq!(drr.dequeue().unwrap().msg, "P2");
        assert_eq!(drr.dequeue().unwrap().msg, "P3");
        assert_eq!(drr.dequeue().unwrap().msg, "P4");
    }

    #[test]
    fn test_integration_ttl_expiration_drop() {
        // 底层队列 max_latency = 100ms
        let classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut drr = ClassDrrQdisc::new(classifier, real_qdisc_factory());

        // P1 到达时间设为 150ms 前 (已过期)
        drr.enqueue(make_aged_pkt("P1-Expired", 1, 50, 150));
        // P2 是新包 (保鲜期内)
        drr.enqueue(make_pkt("P2-Fresh", 1, 50));

        // 触发调度 (peek 会顺着调到底层的 peek，从而触发过期清理)
        let peeked = drr.peek();
        assert!(peeked.is_some());
        assert_eq!(
            peeked.unwrap().msg,
            "P2-Fresh",
            "过期的 P1 应该被跳过，直接看到 P2"
        );

        // 取出新鲜的包
        let out_pkt = drr.dequeue().unwrap();
        assert_eq!(out_pkt.msg, "P2-Fresh");

        // 检查垃圾桶，看看有没有摸尸成功
        let drops = drr.collect_dropped();
        assert_eq!(drops.len(), 1);
        assert_eq!(
            drops[0].msg, "P1-Expired",
            "过期的 P1 应该被收集到了 pending_drops 中"
        );
    }
}

#[cfg(test)]
mod nested_drr_tests {
    use crate::qdisc::leaf::TTLHeadDropQdisc;

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 嵌套测试辅助工具
    // ==========================================

    // 快速构造嵌套场景的包：指定 parent_id 和 child_id
    fn make_nested_pkt(
        msg: &'static str,
        parent_id: u32,
        child_id: usize,
        cost: usize,
    ) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key: parent_id,
            pkt_len: cost,
            cost,
            queue_num: child_id, // 借用 queue_num 作为子 DRR 的 Class ID
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 1. 最底层叶子节点工厂：带有 TTL 和硬性容量限制 (硬限设为 2 方便测试丢包)
    fn leaf_factory() -> Box<dyn Fn() -> Box<dyn Qdisc<&'static str, u32>>> {
        Box::new(|| Box::new(TTLHeadDropQdisc::new(1000, 2)))
    }

    // 2. 内层(子) DRR 工厂：按照 queue_num 分类，每次分配 100 quantum
    fn child_drr_factory() -> Box<dyn Fn() -> Box<dyn Qdisc<&'static str, u32>>> {
        Box::new(|| {
            let classifier =
                Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.queue_num, 100));
            Box::new(ClassDrrQdisc::new(classifier, leaf_factory()))
        })
    }

    // ==========================================
    // 极端情况测试用例
    // ==========================================

    #[test]
    fn test_nested_drr_basic_routing() {
        // 外层(父) DRR：按照 key 分类，每次分配 200 quantum
        let parent_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 200));
        let mut root_drr = ClassDrrQdisc::new(parent_classifier, child_drr_factory());

        // 发送一个包：父节点 1 -> 子节点 10 -> 叶子队列
        root_drr.enqueue(make_nested_pkt("Nested-1", 1, 10, 50));

        let out = root_drr.dequeue().unwrap();
        assert_eq!(out.msg, "Nested-1");
        assert!(root_drr.dequeue().is_none());
    }

    #[test]
    fn test_nested_drr_fairness_tree() {
        let parent_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut root_drr = ClassDrrQdisc::new(parent_classifier, child_drr_factory());

        // 构造复杂的树：
        // Parent 1 下面有 Child 10 和 Child 11
        root_drr.enqueue(make_nested_pkt("P1-C10", 1, 10, 100));
        root_drr.enqueue(make_nested_pkt("P1-C11", 1, 11, 100));
        // Parent 2 下面只有 Child 20，但有两个包
        root_drr.enqueue(make_nested_pkt("P2-C20-A", 2, 20, 100));
        root_drr.enqueue(make_nested_pkt("P2-C20-B", 2, 20, 100));

        // 预期出队顺序 (由于 HashSet/HashMap 迭代顺序不确定，只能验证逻辑模式)
        // 逻辑是：Parent 1 和 Parent 2 交替。
        // 当轮到 Parent 1 时，它的子节点 Child 10 和 Child 11 交替。
        let out1 = root_drr.dequeue().unwrap();
        let out2 = root_drr.dequeue().unwrap();
        let out3 = root_drr.dequeue().unwrap();
        let out4 = root_drr.dequeue().unwrap();

        // 验证 Parent 层级的绝对交替 (P1, P2, P1, P2 或者 P2, P1, P2, P1)
        assert_ne!(out1.key, out2.key, "父节点应该公平交替");
        assert_ne!(out2.key, out3.key, "父节点应该公平交替");
        assert_ne!(out3.key, out4.key, "父节点应该公平交替");
    }

    #[test]
    fn test_nested_drr_deep_deficit_accumulation() {
        // 父节点配额 100，子节点配额 100
        let parent_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut root_drr = ClassDrrQdisc::new(parent_classifier, child_drr_factory());

        // 一个深藏在树底部的巨型包 (cost = 250)
        root_drr.enqueue(make_nested_pkt("Deep-Jumbo", 1, 10, 250));

        // 对照组：父节点 2 发送 3 个普通包 (cost = 100)
        root_drr.enqueue(make_nested_pkt("Normal-P2", 2, 20, 100));
        root_drr.enqueue(make_nested_pkt("Normal-P2-2", 2, 20, 100));
        // ⚠️ 这一步因为底层 hard_limit = 2，会触发 Drop-Head，把 Normal-P2 踢进垃圾桶
        root_drr.enqueue(make_nested_pkt("Normal-P2-3", 2, 20, 100));

        // 过程推演：
        // 轮次1：P2 的队头变成了 Normal-P2-2。P1(C10) 赤字积累到 100，出不来。
        assert_eq!(root_drr.dequeue().unwrap().msg, "Normal-P2-2");

        // 轮次2：P2 剩余的 Normal-P2-3 出队。P1(C10) 赤字积累到 200，依然出不来。
        assert_eq!(root_drr.dequeue().unwrap().msg, "Normal-P2-3");

        // 轮次3：P1 再次获得 100 额度 (总共 300)，巨型包成功出队！
        assert_eq!(root_drr.dequeue().unwrap().msg, "Deep-Jumbo");

        // 🚀 终极验证：摸尸！去大类的垃圾桶里找找，被 Drop-Head 踢掉的 P2 是不是乖乖躺在里面
        let drops = root_drr.collect_dropped();
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].msg, "Normal-P2", "被底层容量限制踢掉的包完美回收");
    }

    #[test]
    fn test_nested_drr_deep_drop_bubbling() {
        // 测试最难的：最底层的垃圾，能不能一层层冒泡(Bubbling)被根节点收集到
        let parent_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut root_drr = ClassDrrQdisc::new(parent_classifier, child_drr_factory());

        // 底层 leaf_factory 的 hard_limit 是 2。我们往同一个叶子队列塞 3 个包！
        root_drr.enqueue(make_nested_pkt("DropMe", 1, 10, 50));
        root_drr.enqueue(make_nested_pkt("Keep1", 1, 10, 50));
        root_drr.enqueue(make_nested_pkt("Keep2", 1, 10, 50)); // 此时 "DropMe" 在最底层被 Drop-Head 踢掉

        // 直接在根节点倒垃圾！
        let drops = root_drr.collect_dropped();

        // 验证：叶子队列产生的垃圾 -> 被子 DRR 摸尸 -> 再被父 DRR 摸尸，完美冒泡
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].msg, "DropMe");

        // 正常出队的应该是保留的两个
        assert_eq!(root_drr.dequeue().unwrap().msg, "Keep1");
        assert_eq!(root_drr.dequeue().unwrap().msg, "Keep2");
    }

    #[test]
    fn test_nested_drr_cascading_cleanup() {
        // 测试级联销毁：当底层的队列空了，子 DRR 会不会销毁它的记录？父 DRR 会不会销毁子 DRR？
        let parent_classifier = Box::new(|ctx: &PacketContext<&'static str, u32>| (ctx.key, 100));
        let mut root_drr = ClassDrrQdisc::new(parent_classifier, child_drr_factory());

        root_drr.enqueue(make_nested_pkt("Pkt-1", 1, 10, 50));

        // 验证结构已创建
        assert_eq!(root_drr.classes.len(), 1);
        assert_eq!(root_drr.active_classes.len(), 1);

        // 掏空队列
        root_drr.dequeue();

        // 验证掏空后，由于状态机的 `prepare_next_ready_class` 会检测到 None 并 remove_class
        // 但注意：remove_class 是在下一次尝试 peek/dequeue 时才会触发清理！
        let is_empty = root_drr.peek().is_none();
        assert!(is_empty);

        // 此时，因为包被拿走，再次 peek 触发了清理逻辑，父节点的 HashMap 应该为空了
        assert_eq!(root_drr.classes.len(), 0, "父级 DRR 应该销毁了空的子 DRR");
    }
}
