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
}

impl<T, K, C> ClassDrrQdisc<T, K, C>
where
    K: Hash + Eq + Clone,
    C: Hash + Eq + Clone
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
        }
    }

    // 状态机：寻找下一个有钱发包的大类
    fn prepare_next_ready_class(&mut self) -> bool {
        loop {
            let class_id = match self.active_classes.pop_front() {
                Some(id) => id,
                None => return false,
            };

            let mut remove_class = false;
            let mut move_to_back = false;

            if let Some(class) = self.classes.get_mut(&class_id) {
                // ✅ 无论 Inner 是什么，只要它实现了 Qdisc，就能 peek
                if let Some(ctx) = class.inner_qdisc.peek() {
                    let len = ctx.cost as i32;

                    if class.deficit < len {
                        // 钱不够，充值排到队尾
                        class.deficit += class.quantum;
                        move_to_back = true;
                    } else {
                        // 钱够，锁定它
                        self.active_classes.push_front(class_id);
                        return true;
                    }
                } else {
                    // 底层队列空了
                    remove_class = true;
                }
            } else {
                continue;
            }

            if remove_class {
                // 不重新插回 active_classes 即可
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
impl<T, K, C> Qdisc<T, K>
    for ClassDrrQdisc<T, K, C>
where
    K: Hash + Eq + Clone,
    C: Hash + Eq + Clone
{

    fn enqueue(
        &mut self,
        ctx: PacketContext<T, K>,
    ) -> Result<(), PacketContext<T, K>> {
        let (class_id, class_quantum ) = (self.classifier)(&ctx);

        // 【极其严谨的借用隔离，避免 Rust 报错】
        let (enqueue_result, is_new_or_was_empty) = {
            let mut was_empty = false;

            let class = match self.classes.entry(class_id.clone()) {
                Entry::Occupied(entry) => {
                    let c = entry.into_mut();
                    if c.inner_qdisc.peek().is_none() {
                        was_empty = true;
                    }
                    c
                }
                Entry::Vacant(entry) => {
                    was_empty = true;
                    entry.insert(ClassBuffer {
                        inner_qdisc: (self.inner_factory)(), // ✅ 动态制造底层的队列
                        deficit: class_quantum,
                        quantum: class_quantum,
                    })
                }
            };

            class.quantum = class_quantum; // 更新大类配额

            // ✅ 把 inner_param 完美透传给底层的入队逻辑
            let res = class.inner_qdisc.enqueue(ctx);
            (res, was_empty)
        };

        // 如果入队成功，且它原本是空的，激活这个大类
        if enqueue_result.is_ok() && is_new_or_was_empty {
            self.active_classes.push_front(class_id);
        }

        enqueue_result
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if !self.prepare_next_ready_class() {
            return None;
        }
        let class_id = self.active_classes.front()?;
        self.classes.get_mut(class_id)?.inner_qdisc.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        if !self.prepare_next_ready_class() {
            return None;
        }

        let class_id = self.active_classes.pop_front()?;
        let class = self.classes.get_mut(&class_id)?;

        // ✅ 拆弹：安全提货，规避底层队列突然变空的风险
        if let Some(ctx) = class.inner_qdisc.dequeue() {
            class.deficit -= ctx.cost as i32;

            if class.inner_qdisc.peek().is_some() {
                self.active_classes.push_front(class_id);
            }
            Some(ctx)
        } else {
            None
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut all_drops = Vec::new();
        for class in self.classes.values_mut() {
            all_drops.extend(class.inner_qdisc.collect_dropped());
        }
        all_drops
    }
}
