use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use std::collections::HashMap;

// ==========================================
// ACK 拦截套管 (Tombstone Wrapper)
// 可以套在任何 Qdisc 外面！
// ==========================================
use std::time::{Duration, Instant};

pub struct TcpAckFilterQdisc<T, K> {
    inner: Box<dyn Qdisc<T, K>>,
    // 🚀 改成 (u32, Instant) 记录最后一次见到这个流的时间
    highest_acks: HashMap<K, (u32, Instant)>,
    dropped: Vec<PacketContext<T, K>>,
    packet_counter: u64, // 🚀 用于触发 GC
}

impl<T, K: Clone + std::hash::Hash + Eq> TcpAckFilterQdisc<T, K> {
    pub fn new(inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            inner,
            highest_acks: HashMap::new(),
            dropped: Vec::new(),
            packet_counter: 0,
        }
    }
}

// 🚀 套管实现：拦截出入动作
impl<T, K: Clone + std::hash::Hash + Eq> Qdisc<T, K> for TcpAckFilterQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>> {
        self.packet_counter += 1;

        // 🚀 每处理 1024 个包，执行一次扫地机器人，清理 120 秒前的死连接
        if self.packet_counter % 1024 == 0 {
            let now = Instant::now();
            self.highest_acks.retain(|_, &mut (_, last_seen)| {
                now.saturating_duration_since(last_seen) < Duration::from_secs(120)
            });
        }

        if ctx.is_pure_ack {
            let now = Instant::now();
            let entry = self
                .highest_acks
                .entry(ctx.key.clone())
                .or_insert((ctx.tcp_ack_num, now));

            // 更新最高 ACK 和最新时间
            if ctx.tcp_ack_num.wrapping_sub(entry.0) as i32 > 0 {
                entry.0 = ctx.tcp_ack_num;
            }
            entry.1 = now; // 🚀 每次看到新 ACK，刷新它的保活时间
        }

        self.inner.enqueue(ctx)
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 由于 peek 不能改变状态，如果队头是一个废弃的 ACK，我们只能假装它是个正常包
        // 这在绝大多数调度器逻辑里是无害的
        self.inner.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        loop {
            let ctx = self.inner.dequeue()?;

            if ctx.is_pure_ack {
                // 🚀 解包时用 &(highest, _) 忽略时间元组
                if let Some(&(highest, _)) = self.highest_acks.get(&ctx.key) {
                    if highest.wrapping_sub(ctx.tcp_ack_num) as i32 > 0 {
                        self.dropped.push(ctx);
                        continue;
                    }
                }
            }
            return Some(ctx);
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut all_drops = std::mem::take(&mut self.dropped);
        all_drops.extend(self.inner.collect_dropped());
        all_drops
    }
}
