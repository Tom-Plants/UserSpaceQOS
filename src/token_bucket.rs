// ================= 极简令牌桶 =================

use std::time::Instant;

// 为了解耦，定义一个令牌桶的 Trait (你的全局或局部 Bucket 都能用)
pub trait TokenBucketLimiter {
    fn can_spend(&mut self, cost: usize) -> bool;
    fn consume(&mut self, cost: usize) -> bool;
}

pub struct TokenBucket {
    pub tokens: f64,   // 当前余额
    rate: f64,     // 速率 (字节/秒)
    capacity: f64, // 桶容量 (突发限制)
    last_update: Instant,
    _name: String,
}

impl TokenBucket {
    pub fn new(rate_bytes_per_sec: f64, burst_bytes: f64, bucket_name: &str) -> Self {
        // println!("[{}]桶初始化：速率({}KB/s) 突发({}KB)", bucket_name, rate_bytes_per_sec / 1024.0, burst_bytes / 1024.0);
        Self {
            tokens: burst_bytes, // 初始给满
            rate: rate_bytes_per_sec,
            capacity: burst_bytes,
            last_update: Instant::now(),
            _name: bucket_name.to_string(),
        }
    }
    fn refill(&mut self) {
        let now = Instant::now();
        // 使用高精度时间差
        let elapsed = now.duration_since(self.last_update).as_secs_f64();

        // 只有时间流逝大于微小阈值才计算，避免浮点误差（虽然Rust f64精度很高，这算是个好习惯）
        if elapsed > 0.0001 {
            let new_tokens = self.rate * elapsed;
            self.tokens = (self.tokens + new_tokens).min(self.capacity);
            self.last_update = now;
        }
    }
}

impl TokenBucketLimiter for TokenBucket {
    fn consume(&mut self, amount: usize) -> bool {
        // 1. 先补水
        self.refill();

        // println!("[{}]当前需要扣除令牌 {}，现有{}", self.name, amount, self.tokens);
        let amount = amount as f64;

        // 2. 再判断
        if self.tokens >= amount {
            self.tokens -= amount;
            // println!("[{}]桶令牌剩余 {}", self.name, self.tokens);
            true
        } else {
            // println!("[{}]桶令牌不足", self.name);
            false
        }
    }

    fn can_spend(&mut self, amount: usize) -> bool {
        self.refill();
        self.tokens >= amount as f64
    }
    
}
