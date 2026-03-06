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

        if !self.active_classes.contains(&class_id) {
            self.active_classes.push_front(class_id);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        loop {
            let class_id = self.active_classes.front()?.clone();

            // 🚀 第一步：在一个独立的作用域里，仅做状态判定！绝不在这里 return 引用！
            let (has_packet, is_affordable) = {
                let class = match self.classes.get_mut(&class_id) {
                    Some(c) => c,
                    None => {
                        self.active_classes.pop_front();
                        continue;
                    }
                };

                if let Some(ctx) = class.inner_qdisc.peek() {
                    (true, class.deficit >= ctx.cost as i32)
                } else {
                    (false, false)
                }
            }; // 👈 离开这个大括号，class 的可变借用被完美释放！

            // 🚀 第二步：根据状态，执行动作或返回
            if has_packet && is_affordable {
                // 钱够货好，重新借用一次并直接返回，生命周期完全合法！
                return self.classes.get_mut(&class_id)?.inner_qdisc.peek();
            }

            // 如果没包，或者钱不够，需要处理状态转移
            let id = self.active_classes.pop_front().unwrap();

            if !has_packet {
                // 货空了：物理超度幽灵
                self.classes.remove(&id);
            } else {
                // 有货但钱不够：充值，并发配到队尾
                if let Some(class) = self.classes.get_mut(&id) {
                    class.deficit += class.quantum;
                }
                self.active_classes.push_back(id);
            }
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 极致盲从：刚 peek 过，队头绝对有钱有货！
        let class_id = self.active_classes.front()?.clone();
        let class = self.classes.get_mut(&class_id)?;

        let ctx = class.inner_qdisc.dequeue()?;

        // 乖乖扣费
        class.deficit -= ctx.cost as i32;

        Some(ctx)
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek(); // 级联打扫
        let mut all_drops = std::mem::take(&mut self.pending_drops);
        for class in self.classes.values_mut() {
            all_drops.extend(class.inner_qdisc.collect_dropped());
        }
        all_drops
    }
}
