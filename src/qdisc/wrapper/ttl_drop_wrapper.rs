use std::time::{Duration, Instant};

use crate::{packet_context::PacketContext, qdisc::Qdisc};

pub struct TtlDropWrapper<T, K> {
    pub inner: Box<dyn Qdisc<T, K>>,
    pub max_latency: Duration,
    pending_expired: Vec<PacketContext<T, K>>,
}

impl<T, K> TtlDropWrapper<T, K> {
    pub fn new(max_latency_ms: u64, inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            inner,
            max_latency: Duration::from_millis(max_latency_ms),
            pending_expired: Vec::new(),
        }
    }
}

impl<T, K> Qdisc<T, K> for TtlDropWrapper<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        self.inner.enqueue(ctx);
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        let now = Instant::now();
        // 🚀 Peek 独占权力：循环排雷，直到挖出新鲜包！
        loop {
            if let Some(ctx) = self.inner.peek() {
                if now.saturating_duration_since(ctx.arrival_time) > self.max_latency {
                    if let Some(dead) = self.inner.dequeue() {
                        self.pending_expired.push(dead);
                    }
                    continue;
                }
                return self.inner.peek();
            }
            return None;
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 盲目信任：因为刚调过 peek，队头绝对合法
        self.inner.dequeue()
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek(); // 级联触发排雷
        let mut drops = std::mem::take(&mut self.pending_expired);
        drops.extend(self.inner.collect_dropped());
        drops
    }
}
