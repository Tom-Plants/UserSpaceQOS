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

        // 【极其严谨的借用隔离，避免 Rust 报错】
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
        class.inner_qdisc.enqueue(ctx);

        // 如果入队成功，且它原本是空的，激活这个大类
        if was_empty {
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

            // 🚀 分离借用，同时掏出包、判断空、收集垃圾
            let (ctx_opt, is_empty, rescued_drops) = {
                let class = self.classes.get_mut(&class_id)?;
                let ctx_opt = class.inner_qdisc.dequeue();

                if let Some(ctx) = &ctx_opt {
                    class.deficit -= ctx.cost as i32;
                }

                let empty = class.inner_qdisc.peek().is_none();
                // 🚀 在可能 remove 它之前，强制掏空垃圾桶
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
