use crate::modifier::PacketModifier;
use crate::packet_context::PacketContext;

// ==========================================
// 真实体积还原化妆师 (True Length Modifier)
// 专治 NFQUEUE 截断拷贝导致的“体重造假”
// ==========================================
pub struct TrueLengthModifier;

impl TrueLengthModifier {
    pub fn new() -> Self {
        Self {}
    }
}

// 只要你的 msg 实现了 AsRef<[u8]>，就能直接读取二进制切片
impl<T: AsRef<[u8]>, K> PacketModifier<T, K> for TrueLengthModifier {
    fn process(&self, ctx: &mut PacketContext<T, K>) {
        let data = ctx.msg.as_ref();

        // 基础防御：连 IP 报头都不够的残次品直接忽略
        if data.is_empty() {
            ctx.cost = 0;
            panic!();
        }

        // 提取 IP 版本号 (第 0 字节的高 4 位)
        let version = data[0] >> 4;

        if version == 4 && data.len() >= 4 {
            let total_length = u16::from_be_bytes([data[2], data[3]]) as usize;
            ctx.pkt_len = total_length;
            ctx.cost = total_length;
        } else {
            panic!()
        }
    }
}
