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
    pub fn new(
        sparse_qdisc: Box<dyn Qdisc<T, K>>,
        bulk_qdisc: Box<dyn Qdisc<T, K>>,
    ) -> Self {
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
impl<T, K> Qdisc<T, K>
    for SparseQdisc<T, K>
where
    K: Hash + Eq + Clone,
{
    fn enqueue(
        &mut self,
        ctx: PacketContext<T, K>,
    ) -> Result<(), PacketContext<T, K>> {
        let key = ctx.key.clone();
        let current_count = self.flow_counts.get(&key).copied().unwrap_or(0);

        if current_count == 0 {
            // =====================================
            // 稀疏流判定！扔进泛型的 VIP 通道
            // =====================================
            match self.sparse_qdisc.enqueue(ctx) {
                Ok(()) => {
                    self.flow_counts.insert(key, 1);
                    Ok(())
                }
                Err(rejected_ctx) => Err(rejected_ctx), // 如果 VIP 通道物理爆满，退回
            }
        } else {
            // =====================================
            // 贪婪流判定！降级踢进底层苦力营
            // =====================================
            match self.bulk_qdisc.enqueue(ctx) {
                Ok(()) => {
                    if let Some(count) = self.flow_counts.get_mut(&key) {
                        *count += 1;
                    }
                    Ok(())
                }
                Err(rejected_ctx) => Err(rejected_ctx),
            }
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if let Some(ctx) = self.sparse_qdisc.peek() {
            return Some(ctx);
        }
        self.bulk_qdisc.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 优先级 1：从 VIP 泛型队列出队
        if let Some(ctx) = self.sparse_qdisc.dequeue() {
            // ✅ 拆弹：安全扣减
            if let Some(count) = self.flow_counts.get_mut(&ctx.key) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    self.flow_counts.remove(&ctx.key);
                }
            }
            return Some(ctx);
        }

        // 优先级 2：从 苦力营泛型队列出队
        if let Some(ctx) = self.bulk_qdisc.dequeue() {
            // ✅ 拆弹：安全扣减
            if let Some(count) = self.flow_counts.get_mut(&ctx.key) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    self.flow_counts.remove(&ctx.key);
                }
            }
            return Some(ctx);
        }

        None
    }

    // ✅ 完美的垃圾收集机制：同时清理两边的超时垃圾！
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        // 1. 去稀疏通道和苦力通道收集垃圾
        let mut drops = self.sparse_qdisc.collect_dropped();
        drops.extend(self.bulk_qdisc.collect_dropped());

        // 2. 擦屁股：只要有包因为超时死了，必须同步扣减计步器
        for dropped_ctx in &drops {
            if let Some(count) = self.flow_counts.get_mut(&dropped_ctx.key) {
                if *count > 0 {
                    *count -= 1;
                }
                if *count == 0 {
                    self.flow_counts.remove(&dropped_ctx.key);
                }
            }
        }

        drops
    }
}
