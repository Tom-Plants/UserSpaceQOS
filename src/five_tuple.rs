// five_tuple.rs
use std::net::Ipv4Addr;

use nfq::Message;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FiveTuple {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub src_port: u16,
    pub dst_port: u16,
}

impl From<&Vec<u8>> for FiveTuple {
    fn from(value: &Vec<u8>) -> Self {
        value.as_slice().into()
    }
}

impl From<&Message> for FiveTuple {
    fn from(value: &Message) -> Self {
        value.get_payload().into()
    }
}

// 为 FiveTuple 实现 From trait
impl From<&[u8]> for FiveTuple {
    fn from(payload: &[u8]) -> Self {
        // 初始化一个空的五元组
        let mut t = FiveTuple {
            src: Ipv4Addr::new(0, 0, 0, 0),
            dst: Ipv4Addr::new(0, 0, 0, 0),
            proto: 0,
            src_port: 0,
            dst_port: 0,
        };

        // 1. 基础长度检查 (IP Header 至少 20 字节)
        if payload.len() < 20 {
            return t;
        }

        // 2. 版本检查 (IPv4)
        // payload[0] 高 4 位是版本
        if (payload[0] >> 4) != 4 {
            return t;
        }

        // 3. 获取 IHL (Header Length)，单位是 32-bit word
        let ihl = (payload[0] & 0x0F) as usize * 4;

        // 如果包长度小于 IHL，说明包是不完整的
        if payload.len() < ihl {
            return t;
        }

        // 4. 解析 IP 层信息
        t.proto = payload[9];
        t.src = Ipv4Addr::from_bits(u32::from_be_bytes([
            payload[12],
            payload[13],
            payload[14],
            payload[15],
        ]));
        t.dst = Ipv4Addr::from_bits(u32::from_be_bytes([
            payload[16],
            payload[17],
            payload[18],
            payload[19],
        ]));

        // 5. 解析传输层端口 (仅 TCP=6 和 UDP=17)
        // 需要确保 payload 长度足够包含端口号 (源端口 + 目的端口 = 4 字节)
        if t.proto == 6 || t.proto == 17 {
            if payload.len() >= ihl + 4 {
                t.src_port = u16::from_be_bytes([payload[ihl], payload[ihl + 1]]);
                t.dst_port = u16::from_be_bytes([payload[ihl + 2], payload[ihl + 3]]);
            }
        }

        t
    }
}
