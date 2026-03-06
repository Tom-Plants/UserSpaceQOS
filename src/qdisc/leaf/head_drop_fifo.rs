use std::collections::VecDeque;

use crate::{packet_context::PacketContext, qdisc::Qdisc};

// ==========================================
// 纯粹的容量限制队列 (不管时间，只管空间)
// ==========================================
pub struct HeadDropFifo<T, K> {
    queue: VecDeque<PacketContext<T, K>>,
    hard_limit: usize,
    dropped: Vec<PacketContext<T, K>>, // 被物理挤出去的包
}

impl<T, K> HeadDropFifo<T, K> {
    pub fn new(hard_limit: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            hard_limit,
            dropped: Vec::new(),
        }
    }
}

impl<T, K> Qdisc<T, K> for HeadDropFifo<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if self.queue.len() >= self.hard_limit {
            if let Some(old_ctx) = self.queue.pop_front() {
                self.dropped.push(old_ctx); // 容量爆了，踢掉队头
            }
        }
        self.queue.push_back(ctx);
    }
    
    fn peek(&mut self) -> Option<&PacketContext<T, K>> { self.queue.front() }
    fn dequeue(&mut self) -> Option<PacketContext<T, K>> { self.queue.pop_front() }
    
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        std::mem::take(&mut self.dropped)
    }
}