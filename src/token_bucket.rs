// ================= 极简令牌桶 =================

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

// 为了解耦，定义一个令牌桶的 Trait (你的全局或局部 Bucket 都能用)
pub trait TokenBucketLimiter {
    fn can_spend(&mut self, cost: usize) -> bool;
    fn consume(&mut self, cost: usize) -> bool;
    /// 桶当前的突发额度（容量）。
    /// HTB 用它当"保留额度"（低优想借全局桶时必须给高优留够这么多）——
    /// 必须**实时**读取，C 变化后才会跟着变；构造时的固定值会让借用永远不成立。
    fn burst(&self) -> f64;
}

pub struct TokenBucket {
    pub tokens: f64, // 当前余额
    rate: f64,       // 速率 (字节/秒)
    capacity: f64,   // 桶容量 (突发限制)
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

        // if self.count == u8::MAX && self._name == "Global" {
        //     println!(
        //         "[{}]当前需要扣除令牌 {}，现有{}",
        //         self._name, amount, self.tokens
        //     );
        //     self.count = 0;
        // } else {
        //     self.count += 1;
        // }
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

    fn burst(&self) -> f64 {
        self.capacity
    }
}

// ================= 运行中可改写速率 =================

impl TokenBucket {
    pub fn rate(&self) -> f64 {
        self.rate
    }
    pub fn burst(&self) -> f64 {
        self.capacity
    }

    /// 动态改速率。两件事必须一起做：
    ///   ① 先按**旧**速率补水，把已经过去的时间算清楚（不然速率切换那一刻会凭空多/少令牌）
    ///   ② 把存量令牌夹到新容量 —— 否则降速的瞬间，手里攒着的旧令牌会立刻放出一发大突发，
    ///      上游排队继续涨，而控制环以为自己已经退让了（这就是"降速不降令牌"那个坑）
    pub fn set_rate(&mut self, rate_bytes_per_sec: f64, burst_bytes: f64) {
        self.refill();
        self.rate = rate_bytes_per_sec;
        self.capacity = burst_bytes;
        if self.tokens > burst_bytes {
            self.tokens = burst_bytes;
        }
    }
}

/// SharedBucket：桶放进 Rc<RefCell<..>>，HtbQdisc 拿走一份克隆、控制环留另一份，
/// 运行中就能改速率。都在主循环同一个线程里访问，RefCell 足够。
#[derive(Clone)]
pub struct SharedBucket(Rc<RefCell<TokenBucket>>);

impl SharedBucket {
    pub fn new(rate_bytes_per_sec: f64, burst_bytes: f64, bucket_name: &str) -> Self {
        Self(Rc::new(RefCell::new(TokenBucket::new(
            rate_bytes_per_sec,
            burst_bytes,
            bucket_name,
        ))))
    }
    pub fn rate(&self) -> f64 {
        self.0.borrow().rate()
    }
    pub fn burst(&self) -> f64 {
        self.0.borrow().burst()
    }
    pub fn set_rate(&self, rate_bytes_per_sec: f64, burst_bytes: f64) {
        self.0.borrow_mut().set_rate(rate_bytes_per_sec, burst_bytes);
    }
}

impl TokenBucketLimiter for SharedBucket {
    fn burst(&self) -> f64 {
        self.0.borrow().burst()
    }
    fn can_spend(&mut self, cost: usize) -> bool {
        self.0.borrow_mut().can_spend(cost)
    }
    fn consume(&mut self, cost: usize) -> bool {
        self.0.borrow_mut().consume(cost)
    }
}
