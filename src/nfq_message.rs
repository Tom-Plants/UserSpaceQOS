use nfq::Message;

pub struct NfqMessage(Message);

impl AsRef<[u8]> for NfqMessage {
    fn as_ref(&self) -> &[u8] {
        self.0.get_payload()
    }
}

impl From<Message> for NfqMessage {
    fn from(value: Message) -> Self {
        Self(value)
    }
}

impl From<NfqMessage> for Message {
    fn from(value: NfqMessage) -> Self {
        value.0
    }
}
