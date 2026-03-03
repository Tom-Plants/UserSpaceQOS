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
        scheduler::{ClassDrrQdisc, DualFairQdisc, PrioQdisc, SparseQdisc},
        wrapper::{MonitorQdisc, RateLimitQdisc, TcpAckFilterQdisc},
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

    let high_priority_rate = 6.0 * 1000.0 * 1000.0 / 8.0;
    let high_priority_burst = 1024.0 * 80.0;
    let high_priority_bucket =
        TokenBucket::new(high_priority_rate, high_priority_burst, "high_priority");

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
        let sparse_leaf: Box<dyn Qdisc<Message, FiveTuple>> = Box::new(MonitorQdisc::new(
            "Sparse",
            Box::new(TTLHeadDropQdisc::new(10, 2048)),
        ));
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
        let class_bulk_leaf_ack_filter = Box::new(MonitorQdisc::new(
            "Bulk",
            Box::new(TcpAckFilterQdisc::new(class_bulk_leaf)),
        ));
        Box::new(MonitorQdisc::new(
            "LOW",
            Box::new(SparseQdisc::new(sparse_leaf, class_bulk_leaf_ack_filter)),
        ))
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

    let root = RateLimitQdisc::new(
        Box::new(PrioQdisc::new(
            Box::new(RateLimitQdisc::new(
                high_qdisc,
                high_priority_bucket,
                Box::new(|_| 0),
            )),
            default_qdisc,
            Box::new(|ctx| ctx.queue_num == 2 || ctx.queue_num == 3),
        )),
        global_bucket,
        Box::new(move |ctx| match ctx.queue_num {
            2 | 3 => 0,
            _ => high_priority_burst as usize,
        }),
    );

    // 3. 构建顶级 HTB 限速网关，并【注入分类器闭包】！
    // let root_htb: RootHtbQdisc<Message, FiveTuple, TokenBucket, TokenBucket> = RootHtbQdisc::new(
    //     high_qdisc,
    //     default_qdisc,
    //     global_bucket,
    //     high_priority_bucket,
    //     high_priority_burst as usize,
    //     // 🌟 灵魂注入：分类规则！只要是队列 2 (Sunshine)，就是 VIP！
    //     Box::new(|ctx: &PacketContext<Message, FiveTuple>| {
    //         ctx.queue_num == 2 || ctx.queue_num == 3
    //     }),
    // );

    // 4. 最外层套上监控大屏
    let mut pipeline = root;

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
