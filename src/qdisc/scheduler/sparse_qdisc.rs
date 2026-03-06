use std::collections::HashMap;
use std::hash::Hash;

use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

// ==========================================
// 智能稀疏流识别调度器 (完全泛型版)
// ==========================================
pub struct SparseQdisc<T, K> {
    // 1. VIP 专用高速通道 (✅ 现在它也是一个泛型的 Qdisc 了！)
    sparse_qdisc: Box<dyn Qdisc<T, K>>,

    // 2. 苦力营 (底层平民队列)
    bulk_qdisc: Box<dyn Qdisc<T, K>>,

    // 3. 全知计步器
    flow_counts: HashMap<K, usize>,
}

impl<T, K> SparseQdisc<T, K> {
    pub fn new(sparse_qdisc: Box<dyn Qdisc<T, K>>, bulk_qdisc: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            sparse_qdisc,
            bulk_qdisc,
            flow_counts: HashMap::new(),
        }
    }
}

// ==========================================
// 实现统一的 Qdisc 接口
// ✅ 参数变成了元组 (SparseParam, BulkParam)，分别喂给两个底层
// ==========================================
impl<T, K: Hash + Eq + Clone> Qdisc<T, K> for SparseQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        let key = ctx.key.clone();
        let count = self.flow_counts.get(&key).copied().unwrap_or(0);

        if count == 0 {
            self.sparse_qdisc.enqueue(ctx);
            self.flow_counts.insert(key, 1);
        } else {
            self.bulk_qdisc.enqueue(ctx);
            if let Some(c) = self.flow_counts.get_mut(&key) {
                *c += 1;
            }
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if self.sparse_qdisc.peek().is_some() {
            self.sparse_qdisc.peek()
        } else {
            self.bulk_qdisc.peek()
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 盲提货
        let ctx_opt = if self.sparse_qdisc.peek().is_some() {
            self.sparse_qdisc.dequeue()
        } else {
            self.bulk_qdisc.dequeue()
        };

        // 安全扣减计步器
        if let Some(ctx) = &ctx_opt {
            if let Some(count) = self.flow_counts.get_mut(&ctx.key) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    self.flow_counts.remove(&ctx.key);
                }
            }
        }
        ctx_opt
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek(); // 级联打扫
        let mut drops = self.sparse_qdisc.collect_dropped();
        drops.extend(self.bulk_qdisc.collect_dropped());

        // 清理死包的计步器
        for dead in &drops {
            if let Some(count) = self.flow_counts.get_mut(&dead.key) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    self.flow_counts.remove(&dead.key);
                }
            }
        }
        drops
    }
}
