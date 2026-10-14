use nfq::Message;

/// 数据包的两种来路：
///   Nfq —— NFQUEUE 拷上来的（只有 128 字节，长度靠 IP 头反推）
///   Tun —— TUN 读上来的**完整**裸 IP 包
pub enum Carrier {
    Nfq(Message),
    Tun(Vec<u8>),
}

pub struct NfqMessage(Carrier);

impl AsRef<[u8]> for NfqMessage {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Carrier::Nfq(m) => m.get_payload(),
            Carrier::Tun(v) => v.as_slice(),
        }
    }
}

impl From<Message> for NfqMessage {
    fn from(value: Message) -> Self {
        Self(Carrier::Nfq(value))
    }
}

impl NfqMessage {
    pub fn from_tun(value: Vec<u8>) -> Self {
        Self(Carrier::Tun(value))
    }
    /// TUN 版用：把完整报文取回来写进另一个 tun
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self.0 {
            Carrier::Tun(v) => Some(v),
            Carrier::Nfq(_) => None,
        }
    }
}

impl From<NfqMessage> for Message {
    fn from(value: NfqMessage) -> Self {
        match value.0 {
            Carrier::Nfq(m) => m,
            Carrier::Tun(_) => panic!("TUN 报文不能当成 NFQUEUE 消息回 verdict"),
        }
    }
}
