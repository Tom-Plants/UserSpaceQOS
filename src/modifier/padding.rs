use crate::{modifier::PacketModifier, packet_context::PacketContext};

// ==========================================
// 1. 加密对齐修改器
// ==========================================
pub struct PaddingModifier {
    block_size: usize,
}
impl PaddingModifier {
    pub fn new(block_size: usize) -> Self { Self { block_size } }
}
impl<T, K> PacketModifier<T, K> for PaddingModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        // 直接修改 ctx.cost，不需要关心 queue_num！
        ctx.cost = ((ctx.cost + self.block_size - 1) / self.block_size) * self.block_size;
    }
}