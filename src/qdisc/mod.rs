use crate::packet_context::PacketContext;

pub mod leaf;
pub mod scheduler;
pub mod wrapper;

pub trait Qdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> ();
    fn peek(&mut self) -> Option<&PacketContext<T, K>>;
    fn dequeue(&mut self) -> Option<PacketContext<T, K>>;
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        Vec::new()
    }
}
