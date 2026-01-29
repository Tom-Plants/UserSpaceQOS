use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::{packet_context::PacketContext, qdisc::Qdisc};

// ==========================================
// 基于时间的 FIFO 调度器
// ==========================================
pub struct FifoQdisc<T, K> {
    queue: VecDeque<PacketContext<T, K>>,
    max_latency: Duration,                     // ✅ 保鲜期
    hard_limit: usize,                         // ✅ 物理兜底
    pending_expired: Vec<PacketContext<T, K>>, // ✅ 垃圾桶
}

impl<T, K> FifoQdisc<T, K> {
    pub fn new(max_latency_ms: u64, hard_limit: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            max_latency: Duration::from_millis(max_latency_ms),
            hard_limit,
            pending_expired: Vec::new(),
        }
    }
}

impl<T, K> Qdisc<T, K> for FifoQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>> {
        if self.queue.len() >= self.hard_limit {
            // Drop-Head: 踢掉最老的，让新的进
            if let Some(old_ctx) = self.queue.pop_front() {
                self.queue.push_back(ctx);
                return Err(old_ctx);
            }
        }
        self.queue.push_back(ctx);
        Ok(())
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        let now = Instant::now();
        while let Some(ctx) = self.queue.front() {
            if now.saturating_duration_since(ctx.arrival_time) > self.max_latency {
                // ✅ 拆弹：安全把过期包拿出来放进垃圾桶
                if let Some(expired_ctx) = self.queue.pop_front() {
                    self.pending_expired.push(expired_ctx);
                }
            } else {
                break;
            }
        }
        self.queue.front()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 先调用 peek 触发垃圾清理
        self.peek()?;
        self.queue.pop_front()
    }

    // ✅ 倒垃圾口
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        std::mem::take(&mut self.pending_expired)
    }
}
