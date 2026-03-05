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
    // ✅ 新增：垃圾中转站
    pending_drops: Vec<PacketContext<T, K>>,
}

impl<T, K> MonitorQdisc<T, K> {
    pub fn new(name: &str, inner: Box<dyn Qdisc<T, K>>) -> Self {
        Self {
            name: name.to_string(),
            inner,
            stats: HashMap::new(),
            last_report: Instant::now(),
            pending_drops: Vec::new(),
        }
    }

    // 🧹 专门负责去底层队列“收尸平账”的核心逻辑
    fn flush_internal_drops(&mut self) {
        let drops = self.inner.collect_dropped();
        for ctx in &drops {
            let stat = self
                .stats
                .entry(ctx.queue_num)
                .or_insert_with(QueueStats::default);

            // 记录一笔丢包
            stat.drop_pkts += 1;

            // 🚨 核心平账：因为它曾经成功入队加了水位，现在死在里面了，必须把水位扣掉！
            stat.backlog_pkts -= 1;
            stat.backlog_bytes -= ctx.cost as i64;

            // 可选：如果你想看暗杀细节，可以解开这行注释
            // println!("🔪 [回收站] 队列 {} 内部释放 {} 字节", ctx.queue_num, ctx.cost);
        }
        self.pending_drops.extend(drops);
    }

    // 打印并重置报表
    fn check_and_report(&mut self) {
        let elapsed = self.last_report.elapsed();

        if elapsed >= Duration::from_secs(1) {
            let now_str = Local::now().format("%H:%M:%S").to_string();

            println!("\n📊 [{}] 监控面板: {}", now_str, self.name);
            println!(
                "---------------------------------------------------------------------------------"
            );
            println!(
                "{:<8} | {:<10} | {:<10} | {:<10} | {:<10} | {:<15}",
                "QueueNum",
                "入队(包/s)",
                "丢弃(包/s)",
                "出队(包/s)",
                "速度(Mbps)",
                "实时积压(包/KB)"
            );
            println!(
                "---------------------------------------------------------------------------------"
            );

            let mut total_in = 0;
            let mut total_drop = 0;
            let mut total_out = 0;
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
                        q_num,
                        stat.in_pkts,
                        stat.drop_pkts,
                        stat.out_pkts,
                        mbps,
                        stat.backlog_pkts,
                        backlog_kb
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

            println!(
                "---------------------------------------------------------------------------------"
            );
            println!(
                "{:<8} | {:<10} | {:<10} | {:<10} | {:<10.2} | {:.1}KB 总积压",
                "TOTAL", total_in, total_drop, total_out, total_mbps, total_backlog_kb
            );
            println!(
                "=================================================================================\n"
            );

            self.last_report = Instant::now();
        }
    }
}

// ==========================================
// 3. 实现 Qdisc 接口 (拦截、更新、平账)
// ==========================================
impl<T, K> Qdisc<T, K> for MonitorQdisc<T, K> {
    fn enqueue(&mut self, ctx: PacketContext<T, K>) {
        let q_num = ctx.queue_num;
        let cost = ctx.cost as i64;

        self.inner.enqueue(ctx);
        let stat = self.stats.entry(q_num).or_insert_with(QueueStats::default);

        stat.in_pkts += 1;
        stat.backlog_pkts += 1;
        stat.backlog_bytes += cost;

        self.flush_internal_drops(); // 入队也可能触发内部超时丢包，顺手收尸
        self.check_and_report();
    }

    fn peek(&mut self) -> Option<&PacketContext<T, K>> {
        self.inner.peek()
    }

    fn dequeue(&mut self) -> Option<PacketContext<T, K>> {
        let result = self.inner.dequeue();

        if let Some(ref ctx) = result {
            let stat = self
                .stats
                .entry(ctx.queue_num)
                .or_insert_with(QueueStats::default);
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
        self.flush_internal_drops();
        std::mem::take(&mut self.pending_drops)
    }
}

#[cfg(test)]
mod monitor_qdisc_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Instant;

    // ==========================================
    // 辅助工具
    // ==========================================

    fn make_pkt(msg: &'static str, queue_num: usize, cost: usize) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key: 1, // Monitor 不看 key，只看 queue_num
            pkt_len: cost,
            cost,
            queue_num,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 专属 Mock 队列：硬性容量限制，专门用来触发丢包测试 Monitor 的“平账”能力
    struct MockLimitQdisc {
        queue: VecDeque<PacketContext<&'static str, u32>>,
        limit: usize,
        drops: Vec<PacketContext<&'static str, u32>>,
    }
    impl MockLimitQdisc {
        fn new(limit: usize) -> Self {
            Self { queue: VecDeque::new(), limit, drops: Vec::new() }
        }
    }
    impl Qdisc<&'static str, u32> for MockLimitQdisc {
        fn enqueue(&mut self, ctx: PacketContext<&'static str, u32>) {
            if self.queue.len() >= self.limit {
                if let Some(drop) = self.queue.pop_front() {
                    self.drops.push(drop); // 触发 Drop-Head
                }
            }
            self.queue.push_back(ctx);
        }
        fn peek(&mut self) -> Option<&PacketContext<&'static str, u32>> { self.queue.front() }
        fn dequeue(&mut self) -> Option<PacketContext<&'static str, u32>> { self.queue.pop_front() }
        fn collect_dropped(&mut self) -> Vec<PacketContext<&'static str, u32>> {
            std::mem::take(&mut self.drops)
        }
    }

    // ==========================================
    // 财务核算测试用例
    // ==========================================

    #[test]
    fn test_monitor_basic_accounting() {
        // 容量给够，不触发丢包
        let inner = Box::new(MockLimitQdisc::new(10));
        let mut monitor = MonitorQdisc::new("eth0-mon", inner);

        // 队列 10 进两个包
        monitor.enqueue(make_pkt("P1", 10, 100));
        monitor.enqueue(make_pkt("P2", 10, 200));

        // 验证记账：入队应该增加 in_pkts 和 backlog
        let stat = monitor.stats.get(&10).unwrap();
        assert_eq!(stat.in_pkts, 2, "收到 2 个包");
        assert_eq!(stat.backlog_pkts, 2, "积压 2 个包");
        assert_eq!(stat.backlog_bytes, 300, "积压 300 字节");
        assert_eq!(stat.out_pkts, 0);

        // 出队一个包
        let out = monitor.dequeue().unwrap();
        assert_eq!(out.msg, "P1");

        // 验证平账：出队后 backlog 应该扣除，out_pkts 增加
        let stat_after = monitor.stats.get(&10).unwrap();
        assert_eq!(stat_after.out_pkts, 1, "发出 1 个包");
        assert_eq!(stat_after.out_bytes, 100.0, "发出 100 字节");
        assert_eq!(stat_after.backlog_pkts, 1, "积压还剩 1 个包");
        assert_eq!(stat_after.backlog_bytes, 200, "积压还剩 200 字节");
    }

    #[test]
    fn test_monitor_drop_compensation() {
        // 🚨 核心测试：测试底层丢包时的“平账”逻辑
        // 底层队列容量只有 2
        let inner = Box::new(MockLimitQdisc::new(2));
        let mut monitor = MonitorQdisc::new("eth0-mon", inner);

        // 塞入 3 个包，第 3 个包入队时，底层会把第 1 个包踢进垃圾桶
        monitor.enqueue(make_pkt("P1", 20, 100)); // 被踢
        monitor.enqueue(make_pkt("P2", 20, 100)); // 存活
        monitor.enqueue(make_pkt("P3", 20, 100)); // 存活

        // 取出状态报表
        let stat = monitor.stats.get(&20).unwrap();

        // 见证奇迹的时刻：
        assert_eq!(stat.in_pkts, 3, "历史总共入队尝试了 3 个");
        assert_eq!(stat.drop_pkts, 1, "成功察觉到底层丢了 1 个包");
        
        // 最关键的平账验证：(3个入队) - (1个丢弃) = (当前真实积压水位)
        assert_eq!(stat.backlog_pkts, 2, "物理水位被精准修正为 2");
        assert_eq!(stat.backlog_bytes, 200, "物理水位字节数被精准修正为 200");

        // 主动收取外层垃圾
        let drops = monitor.collect_dropped();
        assert_eq!(drops.len(), 1, "外部也应该能顺利拿到被丢的包");
        assert_eq!(drops[0].msg, "P1");
    }

    #[test]
    fn test_monitor_multi_queue_isolation() {
        let inner = Box::new(MockLimitQdisc::new(10));
        let mut monitor = MonitorQdisc::new("eth0-mon", inner);

        // 分别向队列 100 和 200 发包
        monitor.enqueue(make_pkt("Q100-P1", 100, 50));
        monitor.enqueue(make_pkt("Q200-P1", 200, 60));
        monitor.enqueue(make_pkt("Q200-P2", 200, 70));

        assert_eq!(monitor.stats.len(), 2, "应该生成了两个独立的账本");

        let stat_100 = monitor.stats.get(&100).unwrap();
        assert_eq!(stat_100.in_pkts, 1);
        assert_eq!(stat_100.backlog_bytes, 50);

        let stat_200 = monitor.stats.get(&200).unwrap();
        assert_eq!(stat_200.in_pkts, 2);
        assert_eq!(stat_200.backlog_bytes, 130);
    }
}