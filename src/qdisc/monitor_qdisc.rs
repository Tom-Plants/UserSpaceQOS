use chrono::Local;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::packet_context::PacketContext;
use crate::qdisc::Qdisc;

// ==========================================
// 1. 升维的队列统计表 (速率 + 积压水位)
// ==========================================
#[derive(Default)]
struct QueueStats {
    // 📈 瞬时速率 (每秒清零)
    in_pkts: u64,
    drop_pkts: u64,
    out_pkts: u64,
    out_bytes: f64,

    // 🌊 实时积压水位 (永远不清零，真实的物理库存)
    backlog_pkts: i64,
    backlog_bytes: i64,
}

// ==========================================
// 2. 高级监控黑盒
// ==========================================
pub struct MonitorQdisc<T, K> {
    name: String,
    pub inner: Box<dyn Qdisc<T, K>>,
    stats: HashMap<usize, QueueStats>,
    last_report: Instant,
}

impl<T, K> MonitorQdisc<T, K> {
    pub fn new(name: &str, inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            name: name.to_string(),
            inner,
            stats: HashMap::new(),
            last_report: Instant::now(),
        }
    }

    // 🧹 专门负责去底层队列“收尸平账”的核心逻辑
    fn flush_internal_drops(&mut self) {
        let drops = self.inner.collect_dropped();
        for ctx in drops {
            let stat = self.stats.entry(ctx.queue_num).or_insert_with(QueueStats::default);
            
            // 记录一笔丢包
            stat.drop_pkts += 1; 
            
            // 🚨 核心平账：因为它曾经成功入队加了水位，现在死在里面了，必须把水位扣掉！
            stat.backlog_pkts -= 1;
            stat.backlog_bytes -= ctx.cost as i64;
            
            // 可选：如果你想看暗杀细节，可以解开这行注释
            // println!("🔪 [回收站] 队列 {} 内部释放 {} 字节", ctx.queue_num, ctx.cost);
        }
    }

    // 打印并重置报表
    fn check_and_report(&mut self) {
        let elapsed = self.last_report.elapsed();

        if elapsed >= Duration::from_secs(1) {
            let now_str = Local::now().format("%H:%M:%S").to_string();

            println!("\n📊 [{}] 监控面板: {}", now_str, self.name);
            println!("---------------------------------------------------------------------------------");
            println!(
                "{:<8} | {:<10} | {:<10} | {:<10} | {:<10} | {:<15}",
                "QueueNum", "入队(包/s)", "丢弃(包/s)", "出队(包/s)", "速度(Mbps)", "实时积压(包/KB)"
            );
            println!("---------------------------------------------------------------------------------");

            let mut total_in = 0; let mut total_drop = 0; let mut total_out = 0;
            let mut total_bytes = 0.0;
            let mut total_backlog_bytes = 0;

            let mut sorted_queues: Vec<_> = self.stats.keys().cloned().collect();
            sorted_queues.sort_unstable();

            // ⚠️ 核心改造 1：不用 stats.clear()，而是手动清空“速率”，保留“积压水位”
            for q_num in sorted_queues {
                if let Some(stat) = self.stats.get_mut(&q_num) {
                    let mbps = (stat.out_bytes * 8.0) / 1_000_000.0 / elapsed.as_secs_f64();
                    let backlog_kb = stat.backlog_bytes as f64 / 1024.0;

                    println!(
                        "{:<8} | {:<10} | {:<10} | {:<10} | {:<10.2} | {}包 / {:.1}KB",
                        q_num, stat.in_pkts, stat.drop_pkts, stat.out_pkts, mbps, stat.backlog_pkts, backlog_kb
                    );

                    total_in += stat.in_pkts;
                    total_drop += stat.drop_pkts;
                    total_out += stat.out_pkts;
                    total_bytes += stat.out_bytes;
                    total_backlog_bytes += stat.backlog_bytes;

                    // 只清空每秒的增量统计
                    stat.in_pkts = 0;
                    stat.drop_pkts = 0;
                    stat.out_pkts = 0;
                    stat.out_bytes = 0.0;
                }
            }

            let total_mbps = (total_bytes * 8.0) / 1_000_000.0 / elapsed.as_secs_f64();
            let total_backlog_kb = total_backlog_bytes as f64 / 1024.0;
            
            println!("---------------------------------------------------------------------------------");
            println!(
                "{:<8} | {:<10} | {:<10} | {:<10} | {:<10.2} | {:.1}KB 总积压",
                "TOTAL", total_in, total_drop, total_out, total_mbps, total_backlog_kb
            );
            println!("=================================================================================\n");

            self.last_report = Instant::now();
        }
    }
}

// ==========================================
// 3. 实现 Qdisc 接口 (拦截、更新、平账)
// ==========================================
impl<T, K> Qdisc<T, K> for MonitorQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) -> Result<(), PacketContext<T, K>> {
        let q_num = ctx.queue_num;
        let cost = ctx.cost as i64;
        
        let result = self.inner.enqueue(ctx);
        let stat = self.stats.entry(q_num).or_insert_with(QueueStats::default);

        match result {
            Ok(()) => {
                stat.in_pkts += 1;
                // 注意：哪怕包最后会被 Fifo 丢掉，既然它 enqueue 成功了，我们先把它算进积压水位里
                stat.backlog_pkts += 1;
                stat.backlog_bytes += cost;
            }
            Err(_) => {
                // 这是直接被外层拒绝（比如 HTB 队列满了直接弹回）的情况
                stat.drop_pkts += 1;
            }
        }

        self.flush_internal_drops(); // 入队也可能触发内部超时丢包，顺手收尸
        self.check_and_report();
        result
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        self.inner.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        let result = self.inner.dequeue();

        if let Some(ref ctx) = result {
            let stat = self.stats.entry(ctx.queue_num).or_insert_with(QueueStats::default);
            stat.out_pkts += 1;
            stat.out_bytes += ctx.cost as f64;
            
            // 正常出队，核销积压水位
            stat.backlog_pkts -= 1;
            stat.backlog_bytes -= ctx.cost as i64;
        }

        // 核心修复：清理那些在出队时被 `AckFilterWrapper` 暗杀，或者被 Fifo 超时丢弃的包！
        self.flush_internal_drops(); 
        
        self.check_and_report();
        result
    }

    fn collect_dropped(&mut self) -> Vec<PacketContext<T, K>> {
        // 如果外部有人主动调用，交出去之前也要平账
        let drops = self.inner.collect_dropped();
        for ctx in &drops {
            let stat = self.stats.entry(ctx.queue_num).or_insert_with(QueueStats::default);
            stat.drop_pkts += 1;
            stat.backlog_pkts -= 1;
            stat.backlog_bytes -= ctx.cost as i64;
        }
        drops
    }
}