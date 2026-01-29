use crate::modifier::PacketModifier;
use crate::packet_context::PacketContext;

// ==========================================
// TCP 特征嗅探修改器 (专门负责盖 is_pure_ack 戳)
// ==========================================
pub struct TcpAckModifier;

impl TcpAckModifier {
    pub fn new() -> Self {
        Self {}
    }
}

// 假设泛型 T 是一个可以转换成字节数组的类型 (比如 Vec<u8> 或 Bytes)
impl<T: AsRef<[u8]>, K> PacketModifier<T, K> for TcpAckModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        // 默认先设为 false
        ctx.is_pure_ack = false;
        ctx.tcp_ack_num = 0;

        let data = ctx.msg.as_ref();

        // 1. 最基础的长度防御 (IPv4 头至少 20 字节)
        if data.len() < 20 {
            return;
        }

        // 2. 检查是不是 IPv4 以及协议是不是 TCP (协议号 6)
        // IP 头的第 9 个字节是 Protocol 字段
        if data[0] >> 4 != 4 || data[9] != 6 {
            return;
        }

        // 3. 计算 IP 头长度 (IHL)
        let ihl = (data[0] & 0x0F) as usize * 4;
        if data.len() < ihl + 20 {
            return;
        } // TCP 头至少也是 20 字节

        let tcp_header_start = ihl;
        let tcp_data = &data[tcp_header_start..];

        // 4. 计算 TCP 头长度 (Data Offset 字段在 TCP 头的第 12 字节的高 4 位)
        let data_offset = (tcp_data[12] >> 4) as usize * 4;

        // 5. 核心判断 A：有且仅有 TCP 头，没有应用层 Payload！(也就是纯控制包)
        if data.len() == ihl + data_offset {
            // 6. 核心判断 B：检查 ACK 标志位是否被置为 1 (TCP 头的第 13 字节)
            // ACK flag 是 0x10 (也就是第 5 位)
            if (tcp_data[13] & 0x10) != 0 {
                // 7. 提取 ACK Number (TCP 头的第 8~11 字节，网络字节序 大端)
                let ack_num =
                    u32::from_be_bytes([tcp_data[8], tcp_data[9], tcp_data[10], tcp_data[11]]);

                // 🎯 完美命中！在面单上盖戳！
                ctx.is_pure_ack = true;
                ctx.tcp_ack_num = ack_num;
            }
        }
    }
}
