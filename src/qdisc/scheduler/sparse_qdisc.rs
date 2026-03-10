// sparse_qdisc.rs
use std::collections::HashMap;
use std::hash::Hash;

use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

pub struct SparseQdisc<T, K> {
    sparse_qdisc: Box<dyn Qdisc<T, K>>,
    bulk_qdisc: Box<dyn Qdisc<T, K>>,
    flow_counts: HashMap<K, usize>,
    last_peek_was_sparse: bool, // 🚀 核心：缓存上一次 peek 的决策路径
}

impl<T, K> SparseQdisc<T, K> {
    pub fn new(sparse_qdisc: Box<dyn Qdisc<T, K>>, bulk_qdisc: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            sparse_qdisc,
            bulk_qdisc,
            flow_counts: HashMap::new(),
            last_peek_was_sparse: true,
        }
    }
}

impl<T, K> Qdisc<T, K> for SparseQdisc<T, K>
where
    K: Hash + Eq + Clone,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        let key = ctx.key.clone();
        let current_count = self.flow_counts.get(&key).copied().unwrap_or(0);

        if current_count == 0 {
            self.sparse_qdisc.enqueue(ctx);
            self.flow_counts.insert(key, 1);
        } else {
            self.bulk_qdisc.enqueue(ctx);
            if let Some(count) = self.flow_counts.get_mut(&key) {
                *count += 1;
            }
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 🚀 记录谁赢得了这次 peek
        if self.sparse_qdisc.peek().is_some() {
            self.last_peek_was_sparse = true;
            return self.sparse_qdisc.peek();
        }
        self.last_peek_was_sparse = false;
        self.bulk_qdisc.peek()
    }

    fn dequeue(&mut self) -> PacketContext<T, K> {
        // 🚀 闭着眼睛按照缓存路径提货，杜绝一切二次验证！
        let ctx = if self.last_peek_was_sparse {
            self.sparse_qdisc.dequeue()
        } else {
            self.bulk_qdisc.dequeue()
        };

        if let Some(count) = self.flow_counts.get_mut(&ctx.key) {
            if *count > 0 { *count -= 1; }
            if *count == 0 { self.flow_counts.remove(&ctx.key); }
        }
        ctx
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek();
        let mut drops = self.sparse_qdisc.collect_dropped();
        drops.extend(self.bulk_qdisc.collect_dropped());
        for dropped_ctx in &drops {
            if let Some(count) = self.flow_counts.get_mut(&dropped_ctx.key) {
                if *count > 0 { *count -= 1; }
                if *count == 0 { self.flow_counts.remove(&dropped_ctx.key); }
            }
        }
        drops
    }
}