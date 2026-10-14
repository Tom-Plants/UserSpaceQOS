use std::collections::VecDeque;

use crate::{packet_context::PacketContext, qdisc::Qdisc};

// ==========================================
// 尾丢弃容量限制队列 (队列满时丢弃新到达的包)
// ==========================================
pub struct TailDropFifo<T, K> {
    queue: VecDeque<PacketContext<T, K>>,
    hard_limit: usize,
    dropped: Vec<PacketContext<T, K>>,
}

impl<T, K> TailDropFifo<T, K> {
    pub fn new(hard_limit: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            hard_limit,
            dropped: Vec::new(),
        }
    }
}

impl<T, K> Qdisc<T, K> for TailDropFifo<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        if self.queue.len() >= self.hard_limit {
            self.dropped.push(ctx);
        } else {
            self.queue.push_back(ctx);
        }
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        self.queue.front()
    }

    fn dequeue(&mut self) -> PacketContext<T, K> {
        self.queue
            .pop_front()
            .expect("致命违规：在 peek 未命中时调用了 dequeue！")
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        std::mem::take(&mut self.dropped)
    }
}
