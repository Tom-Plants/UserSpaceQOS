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
impl<T, K> Qdisc<T, K> for SparseQdisc<T, K>
where
    K: Hash + Eq + Clone,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        let key = ctx.key.clone();
        let current_count = self.flow_counts.get(&key).copied().unwrap_or(0);

        if current_count == 0 {
            // =====================================
            // 稀疏流判定！扔进泛型的 VIP 通道
            // =====================================
            self.sparse_qdisc.enqueue(ctx);
            self.flow_counts.insert(key, 1);
        } else {
            // =====================================
            // 贪婪流判定！降级踢进底层苦力营
            // =====================================
            self.bulk_qdisc.enqueue(ctx);
            if let Some(count) = self.flow_counts.get_mut(&key) {
                *count += 1;
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

#[cfg(test)]
mod sparse_qdisc_tests {
    use crate::qdisc::leaf::TTLHeadDropQdisc;

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 辅助工具：复用全真队列
    // ==========================================

    fn make_pkt(msg: &'static str, key: u32) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key,
            pkt_len: 100,
            cost: 100,
            queue_num: 0,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 底层队列：容量上限 2 (用于测试 Drop-Head 和计步器同步)
    fn make_real_qdisc() -> Box<dyn Qdisc<&'static str, u32>> {
        Box::new(TTLHeadDropQdisc::new(1000, 2))
    }

    // ==========================================
    // 稀疏流测试用例
    // ==========================================

    #[test]
    fn test_sparse_basic_separation() {
        let mut sparse = SparseQdisc::new(make_real_qdisc(), make_real_qdisc());

        // Flow 1: 贪婪流，连发 3 个包
        sparse.enqueue(make_pkt("Flow1-Sparse", 1)); // 第1个，应该进 Sparse
        sparse.enqueue(make_pkt("Flow1-Bulk1", 1)); // 第2个，进 Bulk
        sparse.enqueue(make_pkt("Flow1-Bulk2", 1)); // 第3个，进 Bulk (注意：底层容量是2，但分别在两个队列里)

        // Flow 2: 真正的稀疏流，只发 1 个包
        sparse.enqueue(make_pkt("Flow2-Sparse", 2)); // 第1个，进 Sparse

        // 验证内部计步器
        assert_eq!(
            sparse.flow_counts.get(&1),
            Some(&3),
            "Flow 1 共有 3 个包在队列里"
        );
        assert_eq!(sparse.flow_counts.get(&2), Some(&1), "Flow 2 只有 1 个包");

        // 验证出队顺序：优先出 Sparse 队列里的包！
        // 因为 HashMap 遍历顺序不定，Sparse 里的包具体谁先出取决于底层具体实现，
        // 但我们这里用的是简单的队列嵌套，如果 Sparse 底层是 FIFO，那么先入先出。
        let out1 = sparse.dequeue().unwrap();
        let out2 = sparse.dequeue().unwrap();

        // 确保前两个出来的一定是 Sparse 队列里的包！
        let msgs = vec![out1.msg, out2.msg];
        assert!(msgs.contains(&"Flow1-Sparse"));
        assert!(msgs.contains(&"Flow2-Sparse"));

        // Sparse 出完后，才轮到 Bulk
        assert_eq!(sparse.dequeue().unwrap().msg, "Flow1-Bulk1");
        assert_eq!(sparse.dequeue().unwrap().msg, "Flow1-Bulk2");
        assert!(sparse.dequeue().is_none());

        // 全部掏空后，计步器应该被完全清理干净，不留内存泄漏
        assert!(sparse.flow_counts.is_empty());
    }

    #[test]
    fn test_sparse_flow_downgrade_and_upgrade() {
        // 测试流的状态转换：从 Sparse -> Bulk -> 恢复为 Sparse
        let mut sparse = SparseQdisc::new(make_real_qdisc(), make_real_qdisc());

        // Flow 1 发两个包
        sparse.enqueue(make_pkt("F1-P1", 1)); // 进 VIP
        sparse.enqueue(make_pkt("F1-P2", 1)); // 被降级进 Bulk

        // 此时 VIP 队列里有 F1-P1，拿走它！
        assert_eq!(sparse.dequeue().unwrap().msg, "F1-P1");

        // 注意：此时 Flow 1 在队列里还有 1 个包 (F1-P2, 在 Bulk 里)。
        // 计步器应该从 2 降到了 1。
        assert_eq!(sparse.flow_counts.get(&1), Some(&1));

        // 如果此时 Flow 1 又来了一个包，它还能进 VIP 吗？
        // 不能！因为它在系统里还有积压（计步器为 1）。必须继续进 Bulk。
        sparse.enqueue(make_pkt("F1-P3", 1));
        assert_eq!(sparse.flow_counts.get(&1), Some(&2));

        // 掏空所有包
        assert_eq!(sparse.dequeue().unwrap().msg, "F1-P2");
        assert_eq!(sparse.dequeue().unwrap().msg, "F1-P3");
        assert!(sparse.flow_counts.is_empty());

        // 当彻底空了以后，Flow 1 再发包，应该重新获得 VIP 待遇
        sparse.enqueue(make_pkt("F1-P4", 1));
        assert_eq!(sparse.dequeue().unwrap().msg, "F1-P4"); // P4 一定能直接出来
    }

    #[test]
    fn test_sparse_dropped_sync_with_counters() {
        // 核心测试：验证 collect_dropped 时，计步器能否精准同步，防止幽灵计数导致流永远被困在 Bulk！
        let mut sparse = SparseQdisc::new(make_real_qdisc(), make_real_qdisc());

        // Flow 1 疯狂发包，塞爆 Bulk 队列！
        sparse.enqueue(make_pkt("VIP", 1)); // VIP队列：[VIP]
        sparse.enqueue(make_pkt("Bulk-1", 1)); // Bulk队列：[Bulk-1]
        sparse.enqueue(make_pkt("Bulk-2", 1)); // Bulk队列：[Bulk-1, Bulk-2]

        // 这一包将触发底层 Drop-Head！Bulk-1 会被物理剔除！
        sparse.enqueue(make_pkt("Bulk-3", 1)); // Bulk队列：[Bulk-2, Bulk-3]

        // 入队完后，计步器傻傻地以为系统里有 4 个包。
        assert_eq!(sparse.flow_counts.get(&1), Some(&4));

        // 🚨 倒垃圾时间！
        // collect_dropped 应该能摸到底层被 Drop-Head 的 Bulk-1，并且把它从计步器里扣掉！
        let drops = sparse.collect_dropped();
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].msg, "Bulk-1");

        // 见证奇迹的时刻：计步器必须被修正为 3！
        assert_eq!(
            sparse.flow_counts.get(&1),
            Some(&3),
            "收集垃圾后，必须同步扣减计步器！"
        );

        // 正常出队，继续扣减
        assert_eq!(sparse.dequeue().unwrap().msg, "VIP"); // 剩余 2
        assert_eq!(sparse.dequeue().unwrap().msg, "Bulk-2"); // 剩余 1
        assert_eq!(sparse.dequeue().unwrap().msg, "Bulk-3"); // 剩余 0，字典清理

        // 完美收官
        assert!(sparse.flow_counts.is_empty());
    }
}
