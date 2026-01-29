use crate::{modifier::PacketModifier, packet_context::PacketContext};

pub struct OverheadModifier {
    overhead_bytes: usize,
}
impl OverheadModifier {
    pub fn new(overhead_bytes: usize) -> Self { Self { overhead_bytes } }
}
impl<T, K> PacketModifier<T, K> for OverheadModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        ctx.cost += self.overhead_bytes * ctx.frames;
    }
}