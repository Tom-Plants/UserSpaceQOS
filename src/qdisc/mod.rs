use crate::packet_context::PacketContext;

mod class_drr_qdisc;
mod drr_qdisc;
mod dual_fair_qdisc;
mod fifo_qdisc;
mod monitor_qdisc;
mod root_htb_qdisc;
mod sparse_qdisc;
mod tcp_ack_filter_qdisc;

pub use class_drr_qdisc::ClassDrrQdisc;
pub use drr_qdisc::DrrQdisc;
pub use dual_fair_qdisc::DualFairQdisc;
pub use fifo_qdisc::FifoQdisc;
pub use monitor_qdisc::MonitorQdisc;
pub use root_htb_qdisc::RootHtbQdisc;
pub use sparse_qdisc::SparseQdisc;
pub use tcp_ack_filter_qdisc::TcpAckFilterQdisc;

pub trait Qdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>>;
    fn peek(&mut self) -> Option<&PacketContext<T, K>>;
    fn dequeue(&mut self) -> Option<PacketContext<T, K>>;
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        Vec::new()
    }
}
