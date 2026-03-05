use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
// 引入模块
mod five_tuple;
mod modifier;
mod nfq_message;
mod packet_context;
mod qdisc;
mod token_bucket;

use five_tuple::FiveTuple;
use nfq::{Message as InnerMessage, Queue, Verdict};
use token_bucket::TokenBucket;

use crate::{
    modifier::{
        FragmentModifier, OverheadModifier, PacketModifier, PaddingModifier, TcpAckModifier,
        TrueLengthModifier,
    },
    nfq_message::NfqMessage as Message,
    packet_context::PacketContext,
    qdisc::{
        Qdisc,
        leaf::TTLHeadDropQdisc,
        scheduler::{ClassDrrQdisc, DualFairQdisc, HtbQdisc, SparseQdisc},
        wrapper::{MonitorQdisc, TcpAckFilterQdisc},
    },
};

const OVERHEAD: usize = 14 + 4 + 20 + 60;
const OVERHEAD2: usize = 18 + 20;

const WG_MTU: usize = 1280;
const ETH_MTU: usize = 1500;
const BATCH_LIMIT: usize = 10000;

fn make_queue(queue_num: usize) -> Result<Queue, std::io::Error> {
    let mut q = Queue::open()?;
    let queue_num: u16 = queue_num as u16;
    q.bind(queue_num)?;
    q.set_copy_range(queue_num, 128)?;
    q.set_queue_max_len(queue_num, 10000)?;
    q.set_nonblocking(true);
    Ok(q)
}

fn main() {
    let global_rate = 6.9 * 1000.0 * 1000.0 / 8.0;
    let global_burst = 1024.0 * 290.0;
    let global_bucket = TokenBucket::new(global_rate, global_burst, "Global");

    let high_priority_rate = 1.0 * 1000.0 * 1000.0 / 8.0;
    let high_priority_burst = 1024.0 * 200.0;
    let high_priority_bucket =
        TokenBucket::new(high_priority_rate, high_priority_burst, "high_priority");

    let low_priority_rate = 0.5 * 1000.0 * 1000.0 / 8.0;
    let low_priority_burst = 1024.0 * 90.0;
    let low_priority_bucket =
        TokenBucket::new(low_priority_rate, low_priority_burst, "low_priority");

    let mut modifiers: HashMap<usize, Vec<Box<dyn PacketModifier<_, _>>>> = HashMap::new();

    for q in [0, 1, 2, 3] {
        modifiers.insert(
            q,
            vec![
                Box::new(TrueLengthModifier::new()),
                Box::new(TcpAckModifier::new()),
                Box::new(PaddingModifier::new(16)),
                Box::new(FragmentModifier::new(WG_MTU)),
                Box::new(OverheadModifier::new(OVERHEAD)),
            ],
        );
    }

    for q in [4, 5] {
        modifiers.insert(
            q,
            vec![
                Box::new(TrueLengthModifier::new()),
                Box::new(TcpAckModifier::new()),
                Box::new(FragmentModifier::new(ETH_MTU)),
                Box::new(OverheadModifier::new(OVERHEAD2)),
            ],
        );
    }

    // 1. 构建高优通道 (VIP)：极其简单的 FIFO，最大排队 15ms，防止卡顿
    // let high_qdisc: Box<dyn Qdisc<String, FiveTuple>> = Box::new(FifoQdisc::new(15, 1024 * 1024));

    // 2. 构建默认通道 (平民)：使用智能稀疏流分离器
    //    - 内部的小包流走 Fifo
    //    - 内部的大文件流走 Drr (公平分配，量子设为 1500)
    let default_qdisc = {
        let sparse_leaf: Box<dyn Qdisc<Message, FiveTuple>> =
            Box::new(TTLHeadDropQdisc::new(10, 2048));
        let class_bulk_leaf: Box<dyn Qdisc<Message, FiveTuple>> = Box::new(ClassDrrQdisc::new(
            Box::new(
                |ctx: &PacketContext<Message, FiveTuple>| match ctx.queue_num {
                    0 | 1 => (ctx.key.dst, 1500),
                    4 | 5 => (ctx.key.src, 1500),
                    _ => panic!(),
                },
            ),
            Box::new(|| {
                Box::new(ClassDrrQdisc::new(
                    Box::new(|ctx: &PacketContext<Message, FiveTuple>| (ctx.key.clone(), 1500)),
                    Box::new(|| Box::new(TTLHeadDropQdisc::new(100, 2048))),
                ))
            }),
        ));
        let class_bulk_leaf_ack_filter = Box::new(TcpAckFilterQdisc::new(class_bulk_leaf));
        Box::new(SparseQdisc::new(sparse_leaf, class_bulk_leaf_ack_filter))
    };

    let high_qdisc = {
        let sparse_leaf: Box<dyn Qdisc<Message, FiveTuple>> =
            Box::new(TTLHeadDropQdisc::new(10, 2048));
        let drr_leaf: Box<dyn Qdisc<Message, FiveTuple>> = Box::new(ClassDrrQdisc::new(
            Box::new(|ctx: &PacketContext<Message, FiveTuple>| (ctx.key.clone(), 1500)),
            Box::new(|| Box::new(TTLHeadDropQdisc::new(100, 2048))),
        ));
        let drr_leaf_ack_filter = Box::new(TcpAckFilterQdisc::new(drr_leaf));
        let long_leaf = Box::new(SparseQdisc::new(sparse_leaf, drr_leaf_ack_filter));
        let short_leaf = Box::new(TTLHeadDropQdisc::new(10, 2048));

        Box::new(DualFairQdisc::new(
            short_leaf,
            long_leaf,
            1500,
            Box::new(|ctx| match ctx.queue_num {
                2 => true,
                3 => false,
                _ => panic!(),
            }),
        ))
    };

    let htb = HtbQdisc::new(
        high_qdisc,
        default_qdisc,
        high_priority_bucket,
        low_priority_bucket,
        global_bucket,
        high_priority_burst as usize,
        low_priority_burst as usize,
        Box::new(|ctx| ctx.queue_num == 2 || ctx.queue_num == 3),
    );

    // 4. 最外层套上监控大屏
    let mut pipeline = MonitorQdisc::new("Root", Box::new(htb));

    let mut queues: Vec<Queue> = (0..6)
        .map(|i| make_queue(i).expect("failed to create queue"))
        .collect();

    loop {
        let mut working = false;

        let mut packet_count = 0;
        loop {
            if packet_count >= BATCH_LIMIT {
                break;
            }
            let mut no_packet = true;
            for i in 0..6 {
                let queue_num = i;
                match queues[i].recv() {
                    Ok(msg) => {
                        working = true;
                        packet_count += 1;
                        no_packet = false;

                        let key = FiveTuple::from(msg.get_payload());

                        let mut ctx = PacketContext {
                            msg: Message::from(msg),
                            key,
                            pkt_len: 0,
                            cost: 0,
                            queue_num,
                            arrival_time: Instant::now(),
                            frames: 1,
                            is_pure_ack: false,
                            tcp_ack_num: 0,
                        };

                        if let Some(modifiers) = modifiers.get(&queue_num) {
                            for modifier in modifiers {
                                modifier.process(&mut ctx);
                            }
                        }

                        pipeline.enqueue(ctx);
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            if no_packet {
                break;
            }
        }

        loop {
            let mut send_work_done = false;

            if let Some(msg) = pipeline.dequeue() {
                send_work_done = true;

                let mut inner_msg: InnerMessage = msg.msg.into();
                inner_msg.set_verdict(Verdict::Accept);
                queues[msg.queue_num].verdict(inner_msg).ok();
            }

            if !send_work_done {
                break;
            } else {
                working = true;
            }
        }

        let expired_pkts = pipeline.collect_dropped();
        if !expired_pkts.is_empty() {
            working = true; // 处理垃圾也是在干活，别睡
            for ctx in expired_pkts {
                let mut msg: InnerMessage = ctx.msg.into(); // 注意这里你结构体里叫 msg
                msg.set_verdict(Verdict::Drop);
                queues[ctx.queue_num].verdict(msg).ok();
            }
        }

        if !working {
            std::thread::sleep(Duration::from_micros(100)); // 稍微缩短 sleep 时间以提高响应
        }
    }
}

#[cfg(test)]
mod ultimate_integration_tests {
    use crate::token_bucket::TokenBucketLimiter;

    use super::*;
    use std::time::Instant;

    // ==========================================
    // 辅助工具：复刻 main.rs 的千层饼拓扑
    // ==========================================

    fn make_pkt(
        msg: &'static str,
        queue_num: usize,
        cost: usize,
        flow_id: u32,
    ) -> PacketContext<&'static str, u32> {
        PacketContext {
            msg,
            key: flow_id, // 用 u32 模拟 FiveTuple
            pkt_len: cost,
            cost,
            queue_num,
            arrival_time: Instant::now(),
            frames: 1,
            is_pure_ack: false,
            tcp_ack_num: 0,
        }
    }

    // 工厂：一比一复刻你 main.rs 里的千层嵌套结构
    fn build_pipeline(
        high_reserve: usize,
        low_reserve: usize,
    ) -> HtbQdisc<&'static str, u32, TokenBucket> {
        // --- 1. 构建 Low Qdisc (default_qdisc) ---
        let low_qdisc = {
            let sparse_leaf: Box<dyn Qdisc<_, _>> = Box::new(MonitorQdisc::new(
                "Sparse",
                Box::new(TTLHeadDropQdisc::new(10, 2048)),
            ));

            let class_bulk_leaf: Box<dyn Qdisc<_, _>> = Box::new(ClassDrrQdisc::new(
                Box::new(|ctx| (ctx.key, 1500)), // 简化版分类器
                Box::new(|| {
                    Box::new(ClassDrrQdisc::new(
                        Box::new(|ctx| (ctx.key, 1500)),
                        Box::new(|| Box::new(TTLHeadDropQdisc::new(100, 2048))),
                    ))
                }),
            ));

            let bulk_monitor = Box::new(MonitorQdisc::new("Bulk", class_bulk_leaf));
            Box::new(MonitorQdisc::new(
                "LOW",
                Box::new(SparseQdisc::new(sparse_leaf, bulk_monitor)),
            ))
        };

        // --- 2. 构建 High Qdisc ---
        let high_qdisc = {
            let sparse_leaf: Box<dyn Qdisc<_, _>> = Box::new(TTLHeadDropQdisc::new(10, 2048));
            let drr_leaf: Box<dyn Qdisc<_, _>> = Box::new(ClassDrrQdisc::new(
                Box::new(|ctx| (ctx.key, 1500)),
                Box::new(|| Box::new(TTLHeadDropQdisc::new(100, 2048))),
            ));
            let long_leaf = Box::new(SparseQdisc::new(sparse_leaf, drr_leaf));
            let short_leaf = Box::new(TTLHeadDropQdisc::new(10, 2048));

            Box::new(DualFairQdisc::new(
                short_leaf,
                long_leaf,
                1500,
                Box::new(|ctx| ctx.queue_num == 2), // q=2进short, q=3进long
            ))
        };

        // --- 3. 构建 HTB Root ---
        // 初始让专属桶没钱，强制去全局桶借款，这样才能测出 Reserve 的威力！
        let high_bucket = TokenBucket::new(0.0, 0.0, "High");
        let low_bucket = TokenBucket::new(0.0, 0.0, "Low");
        let global_bucket = TokenBucket::new(1000.0, 1000.0, "Global");

        HtbQdisc::new(
            high_qdisc,
            low_qdisc,
            high_bucket,
            low_bucket,
            global_bucket,
            high_reserve, // 👈 动态注入
            low_reserve,  // 👈 动态注入
            Box::new(|ctx| ctx.queue_num == 2 || ctx.queue_num == 3),
        )
    }

    // ==========================================
    // 终极测试：复现 BUG 与 验证修复
    // ==========================================

    #[test]
    fn test_main_bug_reproduction_priority_inversion() {
        // ❌ 模拟你 main.rs 里写错的参数：
        // high_reserve(90) < low_reserve(200)
        let mut pipeline = build_pipeline(90, 200);

        // 全局池初始有 1000。
        // 🚀 修正：花掉 800，留下 200 块钱！
        pipeline.global_bucket.consume(800);
        // 此时全局池 = 200

        // 高优包 (q=2) 和 低优包 (q=0) 同时抵达，都要花 100
        pipeline.enqueue(make_pkt("Low-Greedy", 0, 100, 1));
        pipeline.enqueue(make_pkt("High-Starved", 2, 100, 2));

        // 🚨 真正的惨剧发生：
        // High 借款需满足：100 + low_reserve(200) = 300 <= 200 -> 钱不够，拒绝！
        // Low  借款需满足：100 + high_reserve(90)  = 190 <= 200 -> 钱够了，允许提款！
        let out = pipeline.dequeue().unwrap();

        assert_eq!(
            out.msg, "Low-Greedy",
            "Bug复现：因为 Reserve 参数传反，低优包竟然抢在高优前面发出了！高优被饿死！"
        );
    }

    #[test]
    fn test_main_bug_fixed_proper_reserves() {
        // ✅ 修复后的正确参数：
        // 我们想保护高优，所以 high_reserve（要求低优必须留下的钱）设为 200
        // low_reserve（高优必须给低优留下的钱）设为 0，高优可以吸干全局！
        let mut pipeline = build_pipeline(200, 0);

        // 同样把全局池抽到只剩 150
        pipeline.global_bucket.consume(850);

        // 高低优同时抵达，都要花 100
        pipeline.enqueue(make_pkt("Low-Blocked", 0, 100, 1));
        pipeline.enqueue(make_pkt("High-VIP", 2, 100, 2));

        // 🛡️ 护航生效：
        // High 借款需满足：100 + low_reserve(0) <= 150 -> 允许！
        // Low  借款需满足：100 + high_reserve(200) <= 150 -> 拒绝！
        let out = pipeline.dequeue().unwrap();
        assert_eq!(
            out.msg, "High-VIP",
            "Bug修复：高优包成功突围！低优包因为准备金护盾被完美卡住。"
        );

        // 并且，出队的这个包成功穿透了最底层的 TTLHeadDrop -> DualFair -> Htb！
        // 证明这套千层饼嵌套没有任何死锁！
    }

    #[test]
    fn test_main_deep_tree_routing_and_counters() {
        // 验证流量是否按预期分配到了千层饼的各个角落
        let mut pipeline = build_pipeline(200, 0);

        // 给足全局带宽
        pipeline.global_bucket.tokens = 10000.0;

        // 发送到 q=2 (High -> DualFair(Short))
        pipeline.enqueue(make_pkt("P1-High-Short", 2, 100, 1));
        // 发送到 q=3 (High -> DualFair(Long) -> Sparse)
        pipeline.enqueue(make_pkt("P2-High-Long-Sparse", 3, 100, 2));
        // 发送到 q=0 (Low -> Sparse)
        pipeline.enqueue(make_pkt("P3-Low-Sparse", 0, 100, 3));

        // 因为 DualFair 是轮询的，所以 Htb 阶段 1/2 会先从 High 抽包。
        // 具体是 Short 还是 Long 取决于 DualFair 初始化 turn_a 状态。
        // 但一定前两个是 High！
        let out1 = pipeline.dequeue().unwrap();
        let out2 = pipeline.dequeue().unwrap();
        let out3 = pipeline.dequeue().unwrap();

        let msgs = vec![out1.msg, out2.msg];
        assert!(msgs.contains(&"P1-High-Short"));
        assert!(msgs.contains(&"P2-High-Long-Sparse"));

        // 最后出的一定是 Low 的包
        assert_eq!(out3.msg, "P3-Low-Sparse");
    }
}
