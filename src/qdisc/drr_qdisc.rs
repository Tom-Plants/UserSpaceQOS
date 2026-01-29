use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::time::{Duration, Instant}; // ✅ 引入时间

use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

struct FlowBuffer<T, K> {
    queue: VecDeque<PacketContext<T, K>>,
    deficit: i32,
    quantum: i32,
    group_id: usize,
}

impl<T, K> FlowBuffer<T, K> {
    fn new(quantum: i32, group_id: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            deficit: quantum,
            quantum,
            group_id,
        }
    }
}

pub struct DrrQdisc<T, K> {
    flows: HashMap<K, FlowBuffer<T, K>>,
    active_queue: VecDeque<K>,
    group_flow_counts: HashMap<usize, usize>,

    // ✅ 核心时间参数
    max_latency: Duration,
    hard_limit: usize,

    // ✅ 内部垃圾桶
    pending_expired: Vec<PacketContext<T, K>>,

    quantum: i32,
}

impl<T, K> DrrQdisc<T, K>
where
    K: Hash + Eq + Clone,
{
    // 初始化改为时间阈值 (例如 max_latency_ms: 100, hard_limit: 2048)
    pub fn new(max_latency_ms: u64, hard_limit: usize, quantum: i32) -> Self {
        Self {
            flows: HashMap::new(),
            active_queue: VecDeque::new(),
            max_latency: Duration::from_millis(max_latency_ms),
            hard_limit,
            group_flow_counts: HashMap::new(),
            pending_expired: Vec::new(),
            quantum,
        }
    }

    fn prepare_next_ready_flow(&mut self) -> bool {
        let now = Instant::now();

        loop {
            let key = match self.active_queue.pop_front() {
                Some(k) => k,
                None => return false,
            };

            let mut remove_flow = false;
            let mut move_to_back = false;
            let mut expired_this_round = Vec::new();

            if let Some(flow) = self.flows.get_mut(&key) {
                // ✅ 掐头去尾，清理所有过期包
                while let Some(ctx) = flow.queue.front() {
                    if now.saturating_duration_since(ctx.arrival_time) > self.max_latency {
                        if let Some(expired) = flow.queue.pop_front() {
                            expired_this_round.push(expired);
                        }
                    } else {
                        break; // 遇到新鲜包，停止清理
                    }
                }

                if let Some(ctx) = flow.queue.front() {
                    let len = ctx.pkt_len as i32;
                    if flow.deficit < len {
                        flow.deficit += flow.quantum;
                        move_to_back = true;
                    } else {
                        self.active_queue.push_front(key);
                        self.pending_expired.extend(expired_this_round);
                        return true;
                    }
                } else {
                    remove_flow = true;
                }
            } else {
                continue;
            }

            self.pending_expired.extend(expired_this_round);

            if remove_flow {
                if let Some(flow) = self.flows.remove(&key) {
                    if let Some(count) = self.group_flow_counts.get_mut(&flow.group_id) {
                        if *count > 0 {
                            *count -= 1;
                        }
                    }
                }
            } else if move_to_back {
                self.active_queue.push_back(key);
            }
        }
    }
}

impl<T, K> Qdisc<T, K> for DrrQdisc<T, K>
where
    K: Hash + Eq + Clone,
{
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>> {
        let key = ctx.key.clone();
        let group_id = ctx.queue_num;

        match self.flows.entry(key.clone()) {
            Entry::Occupied(mut entry) => {
                let flow = entry.get_mut();
                flow.quantum = self.quantum;

                // 物理兜底，防止内存撑爆
                if flow.queue.len() >= self.hard_limit {
                    if let Some(drop_ctx) = flow.queue.pop_front() {
                        flow.queue.push_back(ctx);
                        return Err(drop_ctx);
                    }
                }
                flow.queue.push_back(ctx);
            }
            Entry::Vacant(entry) => {
                let mut flow = FlowBuffer::new(self.quantum, group_id);
                flow.queue.push_back(ctx);
                entry.insert(flow);
                self.active_queue.push_front(key);
                *self.group_flow_counts.entry(group_id).or_insert(0) += 1;
            }
        }
        Ok(())
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        if !self.prepare_next_ready_flow() {
            return None;
        }
        let key = self.active_queue.front()?;
        self.flows.get(key)?.queue.front()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        if !self.prepare_next_ready_flow() {
            return None;
        }

        // ✅ 拆弹：用 ? 替代
        let key = self.active_queue.pop_front()?;

        let (ctx, is_empty, group_id) = {
            // ✅ 拆弹：安全获取流和包
            let flow = self.flows.get_mut(&key)?;
            let ctx = flow.queue.pop_front()?;

            let len = ctx.pkt_len as i32;
            flow.deficit -= len;

            (ctx, flow.queue.is_empty(), flow.group_id)
        };

        if is_empty {
            self.flows.remove(&key);
            if let Some(count) = self.group_flow_counts.get_mut(&group_id) {
                if *count > 0 {
                    *count -= 1;
                }
            }
        } else {
            self.active_queue.push_front(key);
        }

        Some(ctx)
    }

    // ✅ 实现 Trait 方法：暴露倒垃圾的口子
    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        std::mem::take(&mut self.pending_expired)
    }
}
