use std::{
    collections::HashMap,
    sync::atomic::AtomicUsize,
    sync::Arc,
    time::{Duration, Instant},
};
// 引入模块
mod five_tuple;
mod modifier;
mod nfq_message;
mod overhead_cal;
mod packet_context;
mod qdisc;
mod rate_control;
mod token_bucket;
mod udp_probe;

use five_tuple::FiveTuple;
use nfq::{Message as InnerMessage, Queue, Verdict};
use rate_control::RttController;
use token_bucket::SharedBucket;

use crate::{
    modifier::{
        FragmentModifier, OverheadModifier, PacketModifier, PaddingModifier, TcpAckModifier,
        TrueLengthModifier,
    },
    nfq_message::NfqMessage as Message,
    packet_context::PacketContext,
    qdisc::{
        Qdisc,
        leaf::HeadDropFifo,
        scheduler::{ClassDrrQdisc, DualFairQdisc, HtbQdisc, SparseQdisc},
        wrapper::{MonitorQdisc, TcpAckFilterQdisc, TtlDropWrapper},
    },
};

const OVERHEAD: usize = 14 + 4 + 20 + 60;
//const OVERHEAD2: usize = 18 + 20 + 10;
const OVERHEAD2: usize = OVERHEAD;

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
    let global_bucket = SharedBucket::new(global_rate, global_burst, "Global");

    let high_priority_rate = 6.0 * 1000.0 * 1000.0 / 8.0;
    let high_priority_burst = 1024.0 * 145.0;
    let high_priority_bucket =
        SharedBucket::new(high_priority_rate, high_priority_burst, "high_priority");

    let low_priority_rate = 0.9 * 1000.0 * 1000.0 / 8.0;
    let low_priority_burst = 1024.0 * 145.0;
    let low_priority_bucket =
        SharedBucket::new(low_priority_rate, low_priority_burst, "low_priority");

    // 🎚️ 容量控制环。HtbQdisc 拿走的是桶的克隆，这里留一份用来在运行中改速率。
    //    构造时按你原来的调参捕获了「各桶占比」和「burst/rate 秒数」，所以缩放不会破坏你的手感。
    //    没设 NFQ_TARGETS 时整个控制环不启用 → 行为与固定速率时一字不差。
    // NFQUEUE 版保持原样：开销仍然是写死的常数
    // （它一次管两条路——进隧道的内层包和出物理口的外层帧，开销本来就不同，
    //  单值自校准会互相污染，所以那边 cal=false，行为与以前一模一样）
    // 两个口径各给一个能被在线改写的每帧开销。
    // NFQUEUE 版不启用自校准（值固定），但类型统一成 Arc<AtomicUsize>，
    // 这样和 TUN 版那套"自校准直接改 modifier"共用同一条代码路径。
    let ovh_wg = Arc::new(AtomicUsize::new(OVERHEAD));
    let ovh_eth = Arc::new(AtomicUsize::new(OVERHEAD2));
    let mut controller = RttController::from_env(
        vec![
            global_bucket.clone(),
            high_priority_bucket.clone(),
            low_priority_bucket.clone(),
        ],
        ovh_wg.clone(),
        false,
    );

    let mut modifiers: HashMap<usize, Vec<Box<dyn PacketModifier<_, _>>>> = HashMap::new();

    for q in [0, 1, 2, 3] {
        modifiers.insert(
            q,
            vec![
                Box::new(TrueLengthModifier::new()),
                Box::new(TcpAckModifier::new()),
                Box::new(PaddingModifier::new(16)),
                Box::new(FragmentModifier::new(WG_MTU)),
                Box::new(OverheadModifier::new(ovh_wg.clone())),
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
                Box::new(OverheadModifier::new(ovh_eth.clone())),
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
            Box::new(TtlDropWrapper::new(10, Box::new(HeadDropFifo::new(2048))));
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
                    Box::new(|| Box::new(HeadDropFifo::new(2048))),
                ))
            }),
        ));
        let class_bulk_leaf_ack_filter = Box::new(TtlDropWrapper::new(
            100,
            Box::new(TcpAckFilterQdisc::new(class_bulk_leaf)),
        ));
        Box::new(SparseQdisc::new(sparse_leaf, class_bulk_leaf_ack_filter))
    };

    let high_qdisc = {
        let sparse_leaf: Box<dyn Qdisc<Message, FiveTuple>> = Box::new(HeadDropFifo::new(2048));
        let drr_leaf: Box<dyn Qdisc<Message, FiveTuple>> = Box::new(ClassDrrQdisc::new(
            Box::new(|ctx: &PacketContext<Message, FiveTuple>| (ctx.key.clone(), 1500)),
            Box::new(|| Box::new(HeadDropFifo::new(2048))),
        ));
        let drr_leaf_ack_filter = Box::new(TcpAckFilterQdisc::new(drr_leaf));
        let long_leaf = Box::new(TtlDropWrapper::new(
            100,
            Box::new(SparseQdisc::new(sparse_leaf, drr_leaf_ack_filter)),
        ));
        let short_leaf = Box::new(TtlDropWrapper::new(10, Box::new(HeadDropFifo::new(2048))));

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
                            path_id: 0,
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
            if pipeline.peek().is_some() {
                let msg = pipeline.dequeue();

                let mut inner_msg: InnerMessage = msg.msg.into();
                inner_msg.set_verdict(Verdict::Accept);
                queues[msg.queue_num].verdict(inner_msg).ok();

                working = true;
            } else {
                break;
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

        // 🎚️ 先把"这一轮处理了多少包"喂给控制环（空载时它不该升速），再问它新容量。
        //    未设 NFQ_TARGETS 时两步都是空操作，零开销。
        controller.add_packets(packet_count as u64);
        controller.tick();

        if !working {
            std::thread::sleep(Duration::from_micros(100)); // 稍微缩短 sleep 时间以提高响应
        }
    }
}
