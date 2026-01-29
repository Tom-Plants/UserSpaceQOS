use crate::packet_context::PacketContext;

mod fragment;
mod overhead;
mod padding;
mod tcp_ack_modifier;

pub use fragment::FragmentModifier;
pub use overhead::OverheadModifier;
pub use padding::PaddingModifier;
pub use tcp_ack_modifier::TcpAckModifier;

pub trait PacketModifier<T, K> {
    fn process(&self, ctx: &mut PacketContext<T, K>);
}
