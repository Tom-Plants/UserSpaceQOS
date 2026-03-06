// tcp_ack_filter_qdisc.rs 终极版
use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct TcpAckFilterQdisc<T, K> {
    inner: Box<dyn Qdisc<T, K>>,
    highest_acks: HashMap<K, (u32, Instant)>,
    dropped: Vec<PacketContext<T, K>>,
    packet_counter: u64,
}

impl<T, K: Clone + std::hash::Hash + Eq> TcpAckFilterQdisc<T, K> {
    pub fn new(inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self { inner, highest_acks: HashMap::new(), dropped: Vec::new(), packet_counter: 0 }
    }
}

impl<T, K: Clone + std::hash::Hash + Eq> Qdisc<T, K> for TcpAckFilterQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        self.packet_counter += 1;

        if self.packet_counter % 1024 == 0 {
            let now = Instant::now();
            self.highest_acks.retain(|_, &mut (_, last_seen)| {
                now.saturating_duration_since(last_seen) < Duration::from_secs(120)
            });
        }

        if ctx.is_pure_ack {
            let now = Instant::now();
            let entry = self.highest_acks.entry(ctx.key.clone()).or_insert((ctx.tcp_ack_num, now));
            if ctx.tcp_ack_num.wrapping_sub(entry.0) as i32 > 0 {
                entry.0 = ctx.tcp_ack_num;
            }
            entry.1 = now;
        }

        self.inner.enqueue(ctx)
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 🚀 核心修改：Peek 承担所有排雷工作！
        loop {
            if let Some(ctx) = self.inner.peek() {
                if ctx.is_pure_ack {
                    if let Some(&(highest, _)) = self.highest_acks.get(&ctx.key) {
                        if highest.wrapping_sub(ctx.tcp_ack_num) as i32 > 0 {
                            // 发现过期 ACK，行使超度权！
                            let dead = self.inner.dequeue().unwrap();
                            self.dropped.push(dead);
                            continue; // 继续查探下一个包
                        }
                    }
                }
                return self.inner.peek(); // 绝对合法，展示给外面
            }
            return None; // 到底了
        }
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🚀 盲目信任提货
        self.inner.dequeue()
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let _ = self.peek(); // 级联打扫
        let mut all_drops = std::mem::take(&mut self.dropped);
        all_drops.extend(self.inner.collect_dropped());
        all_drops
    }
}