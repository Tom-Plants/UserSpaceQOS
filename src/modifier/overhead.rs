use crate::{modifier::PacketModifier, packet_context::PacketContext};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 每帧开销（字节/帧）。**不写死、也不从系统推导** —— 由 overhead_cal.rs 在线学出来，
/// 通过这个共享原子变量实时生效。还没学到就保持 0（等于只按 IP 报文长记账）。
pub struct OverheadModifier {
    overhead: Arc<AtomicUsize>,
}

impl OverheadModifier {
    pub fn new(overhead: Arc<AtomicUsize>) -> Self {
        Self { overhead }
    }
}

impl<T, K> PacketModifier<T, K> for OverheadModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        let per_frame = self.overhead.load(Ordering::Relaxed);
        ctx.cost += per_frame * ctx.frames;
    }
}
