use crate::packet_context::PacketContext;

mod fragment;
mod overhead;
mod padding;
mod tcp_ack_modifier;
mod true_length;

pub use fragment::FragmentModifier;
pub use overhead::OverheadModifier;
pub use padding::PaddingModifier;
pub use tcp_ack_modifier::TcpAckModifier;
pub use true_length::TrueLengthModifier;

pub trait PacketModifier<T, K> {
    fn process(&self, ctx: &mut PacketContext<T, K>);
}
