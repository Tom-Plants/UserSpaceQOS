use std::time::Instant;

#[derive(Debug)]
pub struct PacketContext<T, K> {
    // 1. 核心载体
    pub msg: T, // 数据包实体
    pub key: K, // 流标识 (用于黑盒内部 Hash 分配队列)

    pub pkt_len: usize,
    pub cost: usize, // 计算完OVERHEAD后的数据包长度

    // 3. 路由归还依据 (为 Verdict 准备)
    pub queue_num: usize, // 必须保留！出队后靠它找到对应的队列句柄发 verdict
    pub arrival_time: Instant, // ✅ 新增：记录包进入内存的时刻

    pub frames: usize,
    pub is_pure_ack: bool,
    pub tcp_ack_num: u32,
}
