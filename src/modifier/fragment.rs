use crate::{modifier::PacketModifier, packet_context::PacketContext};

pub struct FragmentModifier {
    mtu: usize,
}
impl FragmentModifier {
    pub fn new(mtu: usize) -> Self { Self { mtu } }
}
impl<T, K> PacketModifier<T, K> for FragmentModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        ctx.frames = ((ctx.cost as f64 / self.mtu as f64).ceil() as usize).max(1);
    }
}