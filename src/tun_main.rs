// tun_main.rs —— TUN 版整形器（配置文件驱动、支持多路）
//
// 摆法（每一条 [[path]] 一对 tun + 策略路由）：
//
//   规则把流量灌进 path 的 tun_in ──(程序 read)──► 整形（共享的 qdisc 树）──(write)──► 该 path 的 tun_out
//                                                                                        │
//                                          内核按主表/策略路由把它送去真正的下一跳 ◄──────┘
//
//   ① 要整形的流量靠**路由**灌进来（不是 iptables 规则）
//   ② 整形完写进该路的 tun_out，内核再按策略路由送回真正的下一跳
//      —— 所以"这条路去哪个上游"完全由 tun_out 决定；同一个上游可以共用同一个 tun_out
//   ③ 程序一死：撤掉指向 tun_in 的那条路由，流量立刻回到原路 —— 失败模式是显式的，不是黑洞
//
// 与老版本的区别：
//   * 路径、分类号、桶的组成（谁吃总量/谁按比例分/burst 时间常数）全部来自配置文件
//   * 支持多对 tun，但**所有路径共享同一个全局桶**（前提：它们汇合处是同一个物理瓶颈）
//   * SIGHUP 热重载：能当场生效的（控制环参数）立即生效，其余明确提示"需重启"
use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod config;
mod five_tuple;
mod modifier;
mod nfq_message;
mod overhead_cal;
mod udp_probe;
mod packet_context;
mod qdisc;
mod qdisc_builder;
mod rate_control;
mod token_bucket;

use config::Config;
use five_tuple::FiveTuple;
use overhead_cal::CalParams;
use rate_control::{ControlParams, RttController};
use token_bucket::SharedBucket;

use crate::{
    modifier::{OverheadModifier, PacketModifier, TcpAckModifier, TrueLengthModifier},
    nfq_message::NfqMessage as Message,
    packet_context::PacketContext,
    qdisc::{wrapper::MonitorQdisc, Qdisc},
};

// ⚠️ 这里原来有 OVERHEAD / OVERHEAD2 / WG_MTU / ETH_MTU 四个写死的常数，已按设计删除：
//    每帧开销不再写死、也不从系统推导，改由 overhead_cal.rs 在线学出来
//    （学不到就保持 0，等于只按 IP 报文长记账）。
const BATCH_LIMIT: usize = 10000;

// ---- tun 开的三个常量（ioctl 的编号在 32/64 位上都是这个值）----
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const IFF_TUN: i16 = 0x0001;
const IFF_NO_PI: i16 = 0x1000;

// ---- SIGHUP：只设一个标志位，真正的重载在主循环里做（信号处理函数里不能干重活）----
static RELOAD: AtomicBool = AtomicBool::new(false);
extern "C" fn on_sighup(_sig: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

/// 自己按 C ABI 定义 ifreq：name[16] + flags(i16) + 补齐到 40 字节。
/// 不直接用 libc::ifreq —— 它在不同目标上字段名/形状不一样（musl 上甚至把整个结构建模成一个 union），
/// 之前用"偏移 16 写 flags"的办法就踩了这个坑（IFF_NO_PI 没生效 → 读到的包带 4 字节 PI 头）。
#[repr(C)]
struct IfReq {
    name: [libc::c_char; 16],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// 把接口名和 flags 写进去
fn ifreq_fill(ifr: &mut IfReq, name: &str) {
    for (i, b) in name.as_bytes().iter().enumerate() {
        ifr.name[i] = *b as libc::c_char;
    }
    ifr.flags = IFF_TUN | IFF_NO_PI;
}

/// 打开一个已存在的 tun 设备（IFF_TUN|IFF_NO_PI：裸 IP 包，不带 4 字节的 PI 头）
fn tun_open(name: &str) -> RawFd {
    let path = b"/dev/net/tun\0";
    let fd = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDWR) };
    if fd < 0 {
        panic!("打不开 /dev/net/tun：{}", std::io::Error::last_os_error());
    }
    if name.len() >= 16 {
        panic!("接口名太长：{}", name);
    }
    let mut ifr: IfReq = unsafe { std::mem::zeroed() };
    ifreq_fill(&mut ifr, name);
    let rc = unsafe {
        libc::ioctl(
            fd,
            TUNSETIFF as _,
            &mut ifr as *mut IfReq as *mut libc::c_void,
        )
    };
    if rc < 0 {
        panic!(
            "ioctl(TUNSETIFF) 失败（设备 {} 存在吗？需要 ip tuntap add）：{}",
            name,
            std::io::Error::last_os_error()
        );
    }
    // 非阻塞：和 NFQUEUE 版一样用忙等 + 100µs 睡眠的节奏
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    fd
}

/// 读一个包。Ok(n) 表示读到 n 字节；Err(errno) 里 EAGAIN(11) 表示当前没包了。
fn tun_read(fd: RawFd, buf: &mut [u8]) -> Result<usize, i32> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1));
    }
    Ok(n as usize)
}

/// 写一个包（注入该路的 tun_out，内核随后按策略路由送出去）
fn tun_write(fd: RawFd, data: &[u8]) -> Result<(), i32> {
    let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1));
    }
    Ok(())
}

/// 一条被整形的路径的运行时状态
struct Path {
    name: String,
    class: usize,
    tun_in: String,
    tun_out: String,
    in_fd: RawFd,
    out_fd: RawFd,
    // 统计
    read_total: u64,
    enq_total: u64,
    written_total: u64,
    write_errs: u64,
    last_wr_errno: i32,
}

fn ctrl_params(c: &config::ControlCfg) -> ControlParams {
    ControlParams {
        targets: c.targets.clone(),
        probe_interval_s: c.probe_interval_s,
        target_delay_ms: c.target_delay_ms,
        band_ms: c.band_ms,
        min_rate_mbps: c.min_rate_mbps,
        max_rate_mbps: c.max_rate_mbps,
        up_gain: c.up_gain,
        down_gain: c.down_gain,
        creep_gain: c.creep_gain,
        loss_threshold: c.loss_threshold,
        loss_threshold_hard: c.loss_threshold_hard,
        down_gain_hard: c.down_gain_hard,
        hold_after_cut_s: c.hold_after_cut_s,
        control_ms: c.control_ms,
        base_window_s: c.base_window_s,
        dry_run: c.dry_run,
    }
}

fn cal_params(c: &config::CalCfg) -> CalParams {
    CalParams {
        state_file: c.state_file.clone(),
        sample_window_s: c.sample_window_s,
        min_samples: c.min_samples,
        min_spread: c.min_spread,
        max_overhead: c.max_overhead,
    }
}

fn main() {
    let cfg_path = std::env::var("QOS_CONFIG").unwrap_or_else(|_| "/root/qos.toml".to_string());
    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("❌ 配置文件有问题，不启动（宁可不起，也不要带着错误配置跑）：\n  {}", e);
            std::process::exit(2);
        }
    };
    println!("📄 配置来自 {}：{} 条路径，pool = {:?}", cfg_path, cfg.path.len(),
             cfg.pool.iter().map(|p| p.name.clone()).collect::<Vec<_>>());

    // ---- 积木：从配置里搭出所有桶（1 个 total 桶 + 任意多个 share 桶）----
    let pool = &cfg.pool[0];
    let total_cfg = pool
        .bucket
        .iter()
        .find(|b| b.role == "total")
        .expect("validate 保证有 total");

    let total_rate = pool.initial_total_mbps * 1_000_000.0 / 8.0;
    let total_burst = total_rate * total_cfg.tau_ms / 1000.0;
    let global_bucket = SharedBucket::new(total_rate, total_burst, &total_cfg.name);

    let mut bucket_map: HashMap<String, SharedBucket> = HashMap::new();
    bucket_map.insert(total_cfg.name.clone(), global_bucket.clone());

    let mut ctrl_buckets: Vec<SharedBucket> = vec![global_bucket.clone()];
    let mut bucket_desc: Vec<String> = vec![format!(
        "{} = {:.2}M/tau {:.0}ms（吃总量）",
        total_cfg.name, pool.initial_total_mbps, total_cfg.tau_ms
    )];

    for b in pool.bucket.iter().filter(|b| b.role == "share") {
        let r = total_rate * b.share;
        let br = r * b.tau_ms / 1000.0;
        let sb = SharedBucket::new(r, br, &b.name);
        bucket_map.insert(b.name.clone(), sb.clone());
        ctrl_buckets.push(sb);
        bucket_desc.push(format!("{} = {:.1}%/tau {:.0}ms", b.name, b.share * 100.0, b.tau_ms));
    }

    println!("🧱 积木（{}）：{}", pool.name, bucket_desc.join(" | "));

    // 🧮 每帧开销：不写死、不从系统推导 —— 从 0（或存档）起，由自校准在线学
    let ovh = Arc::new(AtomicUsize::new(0));
    let mut controller = RttController::from_params(
        ctrl_buckets,
        ovh.clone(),
        cfg.calibration.enabled,
        ctrl_params(&cfg.control),
        cal_params(&cfg.calibration),
    );

    // 📉 UDP 丢包探针：ICMP 被上游豁免（永远 0 丢），只有 UDP 探针能看见"超发丢包"。
    //    对端需跑 qos_udp_echo.py 回声器；udp_probe_targets 为空则不启用。
    if !cfg.control.udp_probe_targets.is_empty() {
        let h = udp_probe::spawn(
            cfg.control.udp_probe_targets.clone(),
            cfg.control.udp_probe_port,
            cfg.control.udp_probe_interval_ms,
            cfg.control.udp_probe_burst_pkts,
            cfg.control.udp_probe_burst_ms,
            cfg.control.udp_probe_pkt_bytes,
        );
        println!(
            "📉 [UDP探针] 启用：目标 {:?} 端口 {} 间隔 {}ms | 微突发 {} 包×{}B 每 {}ms",
            cfg.control.udp_probe_targets, cfg.control.udp_probe_port, cfg.control.udp_probe_interval_ms,
            cfg.control.udp_probe_burst_pkts, cfg.control.udp_probe_pkt_bytes, cfg.control.udp_probe_burst_ms
        );
        rate_control::set_udp_probe(Some(h));
    }

    // 修饰链：真长还原 → 纯 ACK 标记 → 加"自校准学来的每帧开销"。
    // 自动覆盖配置里出现的所有 class 号（以及 0..=5）
    let mut modifiers: HashMap<usize, Vec<Box<dyn PacketModifier<_, _>>>> = HashMap::new();
    let mut all_classes: Vec<usize> = vec![0, 1, 2, 3, 4, 5];
    for p in &cfg.path {
        if !all_classes.contains(&p.class) {
            all_classes.push(p.class);
        }
    }
    for q in all_classes {
        modifiers.insert(
            q,
            vec![
                Box::new(TrueLengthModifier::new()),
                Box::new(TcpAckModifier::new()),
                Box::new(OverheadModifier::new(ovh.clone())),
            ],
        );
    }

    let root_qdisc = qdisc_builder::build_root_qdisc(&cfg.qdisc, &bucket_map, &global_bucket);
    let mut pipeline = MonitorQdisc::new("Root(TUN)", root_qdisc);

    // ---- 打开每条路径的两块 tun ----
    let mut paths: Vec<Path> = Vec::new();
    for p in &cfg.path {
        let in_fd = tun_open(&p.tun_in);
        let out_fd = tun_open(&p.tun_out);
        println!(
            "🔀 路径 [{}]：读 {}（分类 {}）→ 整形 → 写 {}",
            p.name, p.tun_in, p.class, p.tun_out
        );
        paths.push(Path {
            name: p.name.clone(),
            class: p.class,
            tun_in: p.tun_in.clone(),
            tun_out: p.tun_out.clone(),
            in_fd,
            out_fd,
            read_total: 0,
            enq_total: 0,
            written_total: 0,
            write_errs: 0,
            last_wr_errno: 0,
        });
    }

    // SIGHUP → 主循环里重新读配置文件
    let h: extern "C" fn(libc::c_int) = on_sighup;
    unsafe {
        libc::signal(libc::SIGHUP, h as libc::sighandler_t);
    }
    println!("♻️  热重载：改完 {} 发 `kill -HUP <pid>`（控制环参数立即生效，路径/桶需重启）", cfg_path);

    let mut buf = vec![0u8; 65536];
    let mut outq: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
    let mut dropped_total: u64 = 0;
    let mut dropped_ack: u64 = 0;
    let mut dropped_bytes: u64 = 0;
    let mut dropped_size: [u64; 3] = [0; 3];
    let mut last_drop_report = Instant::now();
    // 自校准窗口计数：这一窗真正放行出去的包数 / 原文字节数（IP 报文长之和）
    let mut out_pkts_win: u64 = 0;
    let mut out_bytes_win: u64 = 0;
    let mut cfg = cfg;

    loop {
        let mut working = false;
        let mut packet_count = 0usize;

        // ---- 收：把每条路径的 tun_in 里排着的包都拿出来 ----
        for (pid, p) in paths.iter_mut().enumerate() {
            loop {
                if packet_count >= BATCH_LIMIT {
                    break;
                }
                match tun_read(p.in_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        working = true;
                        packet_count += 1;
                        p.read_total += 1;
                        let raw = buf[..n].to_vec();
                        let key = FiveTuple::from(&raw);
                        let mut ctx = PacketContext {
                            msg: Message::from_tun(raw),
                            key,
                            pkt_len: 0,
                            cost: 0,
                            queue_num: p.class,
                            arrival_time: Instant::now(),
                            frames: 1,
                            is_pure_ack: false,
                            tcp_ack_num: 0,
                            path_id: pid,
                        };
                        if let Some(mods) = modifiers.get(&p.class) {
                            for m in mods {
                                m.process(&mut ctx);
                            }
                        }
                        pipeline.enqueue(ctx);
                        p.enq_total += 1;
                    }
                    Err(_) => break, // EAGAIN：现在没包了
                }
            }
        }

        // ---- 发：把整形器放行的包写进"它自己那条路的" tun_out ----
        loop {
            if pipeline.peek().is_some() {
                let msg = pipeline.dequeue();
                // 自校准原料：这一轮真正放行了多少包、多少原文字节（IP 报文长）
                out_pkts_win += 1;
                out_bytes_win += msg.pkt_len as u64;
                let pid = msg.path_id.min(paths.len().saturating_sub(1));
                if let Some(bytes) = msg.msg.into_bytes() {
                    outq.push_back((pid, bytes));
                }
                working = true;
            } else {
                break;
            }
        }
        while let Some((pid, front)) = outq.front() {
            let fd = paths[*pid].out_fd;
            match tun_write(fd, front.as_slice()) {
                Ok(()) => {
                    paths[*pid].written_total += 1;
                    outq.pop_front();
                }
                Err(e) if e == libc::EAGAIN => break, // 内核还没收下，下一轮再试
                Err(e) => {
                    paths[*pid].write_errs += 1;
                    paths[*pid].last_wr_errno = e;
                    outq.pop_front(); // 写不进去就丢，避免无限堆积
                }
            }
        }

        // ---- 回收整形器内部丢掉/超时的包（TUN 版不需要回 verdict，直接扔掉）----
        let expired = pipeline.collect_dropped();
        if !expired.is_empty() {
            working = true;
            dropped_total += expired.len() as u64;
            for c in &expired {
                if c.is_pure_ack { dropped_ack += 1; }
                dropped_bytes += c.pkt_len as u64;
                let di = if c.pkt_len <= 100 { 0 } else if c.pkt_len <= 500 { 1 } else { 2 };
                dropped_size[di] += 1;
            }
        }

        controller.add_packets(packet_count as u64);
        controller.add_out_traffic(out_pkts_win, out_bytes_win);
        out_pkts_win = 0;
        out_bytes_win = 0;
        controller.tick();

        // ---- ♻️ 热重载 ----
        if RELOAD.swap(false, Ordering::Relaxed) {
            match Config::load(&cfg_path) {
                Ok(new) => {
                    let applied = controller.apply_params(&ctrl_params(&new.control));
                    if applied.is_empty() {
                        println!("♻️ [热重载] 配置已重新读取，但没有可当场生效的改动");
                    } else {
                        println!("♻️ [热重载] 已生效：{}", applied.join("、"));
                    }
                    let mut need_restart: Vec<&str> = Vec::new();
                    if new.control.targets != cfg.control.targets {
                        need_restart.push("探测目标清单");
                    }
                    if new.path.len() != cfg.path.len()
                        || new
                            .path
                            .iter()
                            .zip(cfg.path.iter())
                            .any(|(a, b)| a.tun_in != b.tun_in || a.tun_out != b.tun_out || a.class != b.class)
                    {
                        need_restart.push("路径/tun 集合");
                    }
                    if new.pool != cfg.pool.iter().map(|p| p.clone()).collect::<Vec<_>>() {
                        need_restart.push("桶的组成");
                    }
                    if new.qdisc != cfg.qdisc {
                        need_restart.push("调度树 [qdisc]");
                    }
                    if new.calibration.sample_window_s != cfg.calibration.sample_window_s
                        || new.calibration.min_samples != cfg.calibration.min_samples
                        || new.calibration.min_spread != cfg.calibration.min_spread
                        || new.calibration.state_file != cfg.calibration.state_file
                    {
                        need_restart.push("自校准参数");
                    }
                    if need_restart.is_empty() {
                        println!("♻️ [热重载] 全部生效，无需重启");
                    } else {
                        println!("♻️ [热重载] ⚠️ 这些改动需要重启才生效：{}", need_restart.join("、"));
                    }
                    cfg = new;
                }
                Err(e) => println!("⚠️ [热重载] 读配置失败，保持原样：{}", e),
            }
        }

        if last_drop_report.elapsed() >= Duration::from_secs(5) {
            let mut line = String::new();
            for p in &paths {
                line.push_str(&format!(
                    " [{}] 读{}/写{}{}",
                    p.name,
                    p.read_total,
                    p.written_total,
                    if p.write_errs > 0 {
                        format!("(失败{} errno{})", p.write_errs, p.last_wr_errno)
                    } else {
                        String::new()
                    }
                ));
            }
            println!(
                "🔀 多路{} | 积压队列 {} | 整形器丢 {}(ACK {} / 小包 {} / 中包 {} / 大包 {} / 共 {}B)",
                line,
                outq.len(),
                dropped_total,
                dropped_ack,
                dropped_size[0],
                dropped_size[1],
                dropped_size[2],
                dropped_bytes,
            );
            last_drop_report = Instant::now();
        }

        if !working {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}
