use crate::modifier::PacketModifier;
use crate::packet_context::PacketContext;
use std::sync::atomic::{AtomicU64, Ordering};

/// 统计"解析不了、直接放行"的报文数（永不 panic 的兜底路径）
static ODD_PACKETS: AtomicU64 = AtomicU64::new(0);

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

        // ⚠️ 这里原来两处都是 panic!()。实测它会把整个进程带走：
        //    台架上混进一个解析不了的包 → 程序 panic 退出 → 这条整形路是全网出口，
        //    等于一次全网中断（fail-closed 变现成事故）。而杂包在真实网络里迟早会出现
        //    （IPv6、ARP 残留、被截断的帧、驱动层的怪东西）。
        // 正确姿势：解析不了就 cost=0 直接放行 —— 不整形、不丢包、不崩，并且计数上报。
        if data.len() < 4 || (data[0] >> 4) != 4 {
            ctx.cost = 0;
            ctx.pkt_len = 0;
            ctx.frames = 0;
            let n = ODD_PACKETS.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 1000 == 0 {
                println!(
                    "⚠️  遇到非 IPv4/残缺报文（第 {} 个）：按 cost=0 直接放行，不整形、不丢",
                    n
                );
            }
            return;
        }

        let total_length = u16::from_be_bytes([data[2], data[3]]) as usize;
        ctx.pkt_len = total_length;
        ctx.cost = total_length;
    }
}
