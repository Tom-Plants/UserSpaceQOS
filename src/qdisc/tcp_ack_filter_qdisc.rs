use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;
use std::collections::HashMap;

// ==========================================
// ACK 拦截套管 (Tombstone Wrapper)
// 可以套在任何 Qdisc 外面！
// ==========================================
pub struct TcpAckFilterQdisc<T, K> {
    // 它可以包装任何东西
    inner: Box<dyn Qdisc<T, K>>,

    // 通缉令账本：记录每个 5 元组目前见过的“最高 ACK 号”
    highest_acks: HashMap<K, u32>,
    dropped: Vec<PacketContext<T, K>>,
}

impl<T, K: Clone + std::hash::Hash + Eq> TcpAckFilterQdisc<T, K> {
    pub fn new(inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            inner,
            highest_acks: HashMap::new(),
            dropped: Vec::new(),
        }
    }
}

// 🚀 套管实现：拦截出入动作
impl<T, K: Clone + std::hash::Hash + Eq> Qdisc<T, K> for TcpAckFilterQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>> {
        // 1. 登记通缉令：如果来了一个纯 ACK，更新这个流的最高 ACK 记录
        if ctx.is_pure_ack {
            let current_highest = self
                .highest_acks
                .entry(ctx.key.clone())
                .or_insert(ctx.tcp_ack_num);

            // 🚀 修复：判断新来的 ACK 是否“在未来”（包含回绕处理）
            // 算法：(新包 - 旧包) 强转为有符号的 i32。如果大于 0，说明新包确实比较新！
            if ctx.tcp_ack_num.wrapping_sub(*current_highest) as i32 > 0 {
                *current_highest = ctx.tcp_ack_num;
            }
        }

        // 2. 若无其事地把它塞进底层黑盒去排队
        self.inner.enqueue(ctx)
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        // 由于 peek 不能改变状态，如果队头是一个废弃的 ACK，我们只能假装它是个正常包
        // 这在绝大多数调度器逻辑里是无害的
        self.inner.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        // 🔪 核心特技：在门口设卡暗杀！
        loop {
            // 从底层黑盒提取一个包
            let ctx = self.inner.dequeue()?;

            // 如果它是纯 ACK，我们核对一下通缉令
            if ctx.is_pure_ack {
                if let Some(&highest) = self.highest_acks.get(&ctx.key) {
                    // 🚀 修复：判断当前包是否“在过去”（包含回绕处理）
                    // 算法：(最高记录 - 当前包) 强转为有符号的 i32。如果大于 0，说明当前包比最高记录要老！
                    if highest.wrapping_sub(ctx.tcp_ack_num) as i32 > 0 {
                        // 刺杀！把它扔进垃圾桶
                        // println!("🔪 拦截掉一个过期的脏 ACK: {}", ctx.tcp_ack_num);
                        self.dropped.push(ctx);
                        continue;
                    }
                }
            }

            // 如果是正常包，或者是最新的 ACK，安全放行
            return Some(ctx);
        }
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        let mut all_drops = std::mem::take(&mut self.dropped);
        all_drops.extend(self.inner.collect_dropped());
        all_drops
    }
}
