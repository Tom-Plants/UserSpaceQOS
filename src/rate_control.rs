// rate_control.rs —— 依据多个远端目标的 RTT，动态决定"这条出口现在能给多少"
//
// 为什么是这么设计的（都来自实测，不是猜的）：
//   1) 延迟对"超出瓶颈"极其灵敏：台架上瓶颈 7Mbit，6.8M 时 RTT 还是基线 30ms，
//      7.2M 时排队已经 338ms —— 而丢包仍然是 0。所以"排队时延"是唯一的早期信号。
//   2) 多目标取**最小值**：所有 peer 共用本机这一段出口，各自最后一公里是它们自己的事。
//      台架实测两路探针的延迟增量完全同步，只有恒定偏移 —— 最小值才反映共享段。
//   3) 探针不能被自己整形，否则"越降越堵、越堵越降"（自指坍缩）。本模块只负责测量，
//      免票那一步在 iptables 侧（生产台架里用 -p icmp -j ACCEPT 放在 NFQUEUE 之前）。
//   4) 探针失联 ≠ 不拥堵。拿不到样本时冻结当前速率，绝不上调（fail-safe）。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::overhead_cal::{CalParams, OverheadCal};
use crate::token_bucket::SharedBucket;
use std::sync::atomic::AtomicUsize;

/// 每个目标一条常驻 ping 进程，采样线程把 (时刻, RTT) 塞进共享队列
struct Target {
    label: String,
    pid: i32,
    samples: Arc<Mutex<VecDeque<(Instant, f64)>>>,
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(default)
}

/// 从 ping 的一行输出里抠出 RTT（毫秒）。busybox 与 iputils 两种写法都认。
fn parse_rtt(line: &str) -> Option<f64> {
    let i = line.find("time=")? + "time=".len();
    let rest = &line[i..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let tok = rest[..end].trim().trim_end_matches("ms").trim();
    if let Some(v) = tok.strip_prefix('<') {
        // "time<1 ms"：按半毫秒算，好过当成 0
        return v.trim().parse::<f64>().ok().map(|x| x / 2.0);
    }
    tok.parse::<f64>().ok()
}

fn spawn_target(target: &str, interval_s: f64) -> Option<Target> {
    let mut child = Command::new("ping")
        .arg("-i")
        .arg(format!("{:.2}", interval_s))
        .arg("-W")
        .arg("1")
        .arg(target)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id() as i32;
    let stdout = child.stdout.take()?;
    let samples: Arc<Mutex<VecDeque<(Instant, f64)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let sink = samples.clone();
    std::thread::spawn(move || {
        let mut child = child; // 线程持有它，保证进程活着
        for line in BufReader::new(stdout).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if let Some(rtt) = parse_rtt(&line) {
                let now = Instant::now();
                let mut q = sink.lock().unwrap();
                q.push_back((now, rtt));
                while let Some(&(t, _)) = q.front() {
                    if now.duration_since(t) > Duration::from_secs(600) {
                        q.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }
        let _ = child.wait();
    });
    Some(Target {
        label: target.to_string(),
        pid,
        samples,
    })
}

// ── UDP 丢包探针的全局句柄 ─────────────────────────────────
// ICMP 被上游豁免（永远 0 丢），丢包信号只能靠 UDP 探针：
// 实测同一拥塞时刻 ICMP 0% 丢而 UDP 探针丢 66%。
use std::sync::{OnceLock, RwLock};
static UDP_PROBE: OnceLock<RwLock<Option<crate::udp_probe::UdpProbeHandles>>> = OnceLock::new();

/// 由 tun_main 启动时注入（未配置探针时为 None）
pub fn set_udp_probe(h: Option<crate::udp_probe::UdpProbeHandles>) {
    let l = UDP_PROBE.get_or_init(|| RwLock::new(None));
    if let Ok(mut g) = l.write() { *g = h; }
}

/// 快照：(双目标min总丢包率 0~1, 最近 RTT ms, 窗口内是否存活)
/// 双目标 min(12.6, 13.3) 已滤掉单条腿源头丢包，≤6.45M 时实测为 0.0%，超 6.5M 时升至 10~30%。
fn udp_probe_snapshot() -> Option<(f64, f64, bool)> {
    let l = UDP_PROBE.get()?;
    let g = l.read().ok()?;
    let h = g.as_ref()?;
    Some((h.window_loss_total(6_000), h.rtt_ms(), h.alive()))
}

/// 完整快照：(稳态丢包, RTT, 存活, 真实丢包含突发)
fn udp_probe_snapshot_full() -> Option<(f64, f64, bool, f64)> {
    let l = UDP_PROBE.get()?;
    let g = l.read().ok()?;
    let h = g.as_ref()?;
    Some((h.window_loss(6_000), h.rtt_ms(), h.alive(), h.window_loss_total(6_000)))
}

/// 归因对账用：只看最近 2 秒窗口的新样本
fn udp_probe_recent_loss() -> Option<(f64, bool)> {
    let l = UDP_PROBE.get()?;
    let g = l.read().ok()?;
    let h = g.as_ref()?;
    Some((h.window_loss_total(2_500), h.alive()))
}

/// 砍速后清空旧丢包窗口，从新速率重新记账（一罪不二罚）
fn udp_probe_reset_window() {
    if let Some(l) = UDP_PROBE.get() {
        if let Ok(g) = l.read() {
            if let Some(h) = g.as_ref() {
                h.reset_window();
            }
        }
    }
}

/// 丢包总闸判定：窗口丢包率超过阈值且探针存活
fn loss_high_now(thresh: f64) -> bool {
    match udp_probe_snapshot() {
        Some((wl, _, alive)) => alive && wl > thresh,
        None => false,
    }
}

/// 连续多少拍"排队超标"才允许降速。
/// 远端链路的突发会让排队瞬间冲高又自己退下去，单拍超标就砍 ⇒ 被突发的假象牵着走
/// （生产实测：C 被锯在 10~11.6 之间十几分钟爬不上去）。
/// 8 拍 ≈ 2 秒（control_ms=250 时）。
const SUSTAIN_TICKS: u64 = 8;

/// 控制环的可调参数（配置文件 [control] 段；SIGHUP 热重载覆盖的就是这几项）
#[derive(Debug, Clone)]
pub struct ControlParams {
    pub targets: Vec<String>,
    pub probe_interval_s: f64,
    pub target_delay_ms: f64,
    pub band_ms: f64,
    pub min_rate_mbps: f64,
    pub max_rate_mbps: f64,
    pub up_gain: f64,
    pub down_gain: f64,
    /// UDP 探针滑动窗口丢包率超过这个值 ⇒ 认定"上游超限"（与排队超标同等待遇）。
    /// 为什么需要：上游超限时是"静默丢包不给排队"（实测 8M 流量丢 20% 而 RTT 不动），
    /// 只看排队的环路对此完全失明。
    pub loss_threshold: f64,
    /// 重丢包阈值：超过它 ⇒ 不等观察期，立刻按 down_gain_hard 重砍（快降）
    pub loss_threshold_hard: f64,
    /// 重砍系数（快降用的狠刀）
    pub down_gain_hard: f64,
    /// 空载/轻载时的极慢上爬步长（每拍乘）。
    /// 为什么需要：升速门槛是"放行量 ≥ 0.7×C"，相对 C 自己算 —— C 一旦被砍低，
    /// 门槛反而更难满足，形成死锁（掉到地板就再也爬不回来）。慢爬是唯一的上升通道。
    /// 自限性：爬过真实容量后排队会涨，降速逻辑立刻接管，不会飘。
    pub creep_gain: f64,
    /// 认账（确认"队列是我造成的"）之后**保持**多少秒不许升速。
    /// 为什么要保持而不是快恢复：砍一刀 −8~28%，若立刻 +5%/拍(≈+20%/秒) 弹回去，
    /// 就永远贴着上限来回锯（实测 C=10 时排队长年 12~54ms，串流卡死）。
    /// 保持期内队列真正排空；之后走常规步长 up_gain 慢慢找回去。
    pub hold_after_cut_s: u64,
    pub control_ms: f64,
    pub base_window_s: f64,
    pub dry_run: bool,
}

impl Default for ControlParams {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            probe_interval_s: 0.2,
            target_delay_ms: 8.0,
            band_ms: 3.0,
            min_rate_mbps: 1.0,
            max_rate_mbps: 24.0,
            up_gain: 1.005,
            down_gain: 0.92,
            loss_threshold: 0.05,
            loss_threshold_hard: 0.08,
            down_gain_hard: 0.6,
            creep_gain: 1.0005,
            hold_after_cut_s: 6,
            control_ms: 250.0,
            base_window_s: 600.0,
            dry_run: false,
        }
    }
}

pub struct RttController {
    enabled: bool,
    targets: Vec<Target>,
    /// (桶, 占总量的比例, burst/rate 的秒数) —— 比例与时间常数都在构造时按用户原调参捕获
    buckets: Vec<(SharedBucket, f64, f64)>,
    rate: f64,          // 当前决定的总容量（字节/秒）
    min_rate: f64,
    max_rate: f64,
    target_delay_ms: f64,
    band_ms: f64,
    up_gain: f64,
    down_gain: f64,
    creep_gain: f64,
    /// 只观测不执行：照常算 C 并打日志，但不真去改桶。
    /// 生产上线第一步应该用它跑一天，看"它本来会怎么调"，确认无害再开执行。
    dry_run: bool,
    control_period: Duration,
    base_window: Duration,
    fresh_window: Duration,
    last_tick: Instant,
    last_report: Instant,
    starved: bool,
    /// 本控制周期内主循环处理过的包数（负载门控：空载不许升速）
    packets_since_tick: u64,
    /// 连续"排队超标"的拍数（不足 SUSTAIN_TICKS 不动手）
    over_ticks: u64,
    /// 降速后的冷却截止时刻（避免连续猛砍造成超调）
    cooldown_until: Instant,
    pub shared_delay_ms: f64,
    pub current_rate: f64,
    /// 上一次真正写进桶的速率（用来判"累计漂移"，不能用"本拍变化"）
    last_written: f64,
    /// 🧮 每帧开销的自校准（None = 没启用）
    cal: Option<OverheadCal>,
    /// 本采样窗口内"真正放行出去"的包数 / 原文字节数 —— 自校准的原料
    out_pkts: u64,
    out_bytes: u64,
    cal_clock: Instant,
    /// 慢启动：还没撞到过排队之前用大步长爬（有存档速率时不启用）
    slow_start: bool,
    /// 归因用：上次砍速前的 (速率, 砍时的排队ms, 时刻)。下一拍验证这一刀到底有没有用。
    pre_cut: Option<(f64, f64, f64, Instant)>,
    /// 丢包阈值（来自配置，热重载可改）
    loss_threshold: f64,
    loss_threshold_hard: f64,
    down_gain_hard: f64,
    /// 上次重砍时刻：10 秒内只准一刀，防止一滑到底
    last_hard_cut: Instant,
    /// 归因结论"队列不是我造成的"之后的静默期：期间不砍也不升
    hold_until: Instant,
    /// 认账后保持不升速的时长（秒），来自配置
    hold_after_cut_s: u64,
    /// 砍过之后的快速恢复期：从地板爬回工作点不该让用户等一百分钟
    fast_until: Instant,
    /// 最近一次归因结论（打在面板上，肉眼可见）
    verdict: String,
    /// 本采样窗口内排队的累加值/次数（用均值判定，别用窗口边界的瞬时值赌运气）
    win_delay_sum: f64,
    win_delay_n: u64,
    /// 本控制周期内放行的包数/字节（用来判"我是不是真的在逼近自己的限额"）
    /// 没有它，空载时环路会一路飘到上限——然后自校准的"饱和"判定就全被误杀
    out_pkts_tick: u64,
    out_bytes_tick: u64,
}

impl RttController {
    /// 主循环每轮把"刚处理了多少包"喂进来（负载门控用）
    pub fn add_packets(&mut self, n: u64) {
        if self.enabled {
            self.packets_since_tick += n;
        }
    }

    /// 主循环每轮把"这一轮真正放行出去的包数 与 原文字节数（IP 报文长）"喂进来。
    /// 这是自校准唯一的原料：原文字节速率 ÷ 包速率，两条不同包长的采样一比就得出每帧开销。
    pub fn add_out_traffic(&mut self, packets: u64, raw_bytes: u64) {
        // 这两笔无论有没有开自校准都要记：控制环用它判断"我是不是在逼近自己的限额"
        self.out_pkts_tick += packets;
        self.out_bytes_tick += raw_bytes;
        if self.cal.is_some() {
            self.out_pkts += packets;
            self.out_bytes += raw_bytes;
        }
    }

    /// NFQ_TARGETS 未设置 → 返回一个完全不启用的控制器（行为与原来一模一样）
    /// calibrate=true 时启用"每帧开销自校准"（只有 TUN 版用：它只有一条路径、一个口径）
    pub fn from_env(buckets: Vec<SharedBucket>, ovh: Arc<AtomicUsize>, calibrate: bool) -> Self {
        // 兼容老用法：先从环境变量组装一份参数，再走同一条构造路径
        let targets_csv = std::env::var("NFQ_TARGETS").unwrap_or_default();
        let ctrl = ControlParams {
            targets: targets_csv
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            probe_interval_s: env_f64("NFQ_PROBE_INTERVAL_S", 0.2),
            target_delay_ms: env_f64("NFQ_TARGET_DELAY_MS", 8.0),
            band_ms: env_f64("NFQ_BAND_MS", 3.0),
            min_rate_mbps: env_f64("NFQ_MIN_RATE_MBPS", 1.0),
            max_rate_mbps: env_f64("NFQ_MAX_RATE_MBPS", 24.0),
            up_gain: env_f64("NFQ_UP_GAIN", 1.005),
            down_gain: env_f64("NFQ_DOWN_GAIN", 0.92),
            loss_threshold: env_f64("NFQ_LOSS_THRESHOLD", 0.05),
            loss_threshold_hard: env_f64("NFQ_LOSS_THRESHOLD_HARD", 0.08),
            down_gain_hard: env_f64("NFQ_DOWN_GAIN_HARD", 0.6),
            creep_gain: env_f64("NFQ_CREEP_GAIN", 1.0005),
            hold_after_cut_s: env_f64("NFQ_HOLD_AFTER_CUT_S", 6.0) as u64,
            control_ms: env_f64("NFQ_CONTROL_MS", 250.0),
            base_window_s: env_f64("NFQ_BASE_WINDOW_S", 600.0),
            dry_run: std::env::var("NFQ_DRY_RUN").map(|v| v != "" && v != "0").unwrap_or(false),
        };
        let calp = CalParams {
            state_file: std::env::var("NFQ_STATE_FILE")
                .unwrap_or_else(|_| "/var/lib/qos_shaper/state".to_string()),
            ..CalParams::default()
        };
        Self::from_params(buckets, ovh, calibrate, ctrl, calp)
    }

    /// 从配置文件的参数构造（TUN 版走这条）
    pub fn from_params(
        buckets: Vec<SharedBucket>,
        ovh: Arc<AtomicUsize>,
        calibrate: bool,
        p: ControlParams,
        calp: CalParams,
    ) -> Self {
        let interval = p.probe_interval_s;
        let names = p.targets.clone();

        let total = buckets.first().map(|b| b.rate()).unwrap_or(1.0);
        let sharers: Vec<(SharedBucket, f64, f64)> = buckets
            .into_iter()
            .map(|b| {
                let r = b.rate();
                let ratio = r / total;
                let bps = if r > 0.0 { b.burst() / r } else { 0.05 };
                (b, ratio, bps)
            })
            .collect();

        let mut targets = Vec::new();
        if !names.is_empty() {
            for n in &names {
                match spawn_target(n, interval) {
                    Some(t) => targets.push(t),
                    None => println!("⚠️  [控制环] 探测目标 {} 起不来，跳过", n),
                }
            }
        }

        let enabled = !targets.is_empty();
        if enabled {
            println!(
                "🎚️  [控制环] 启用：目标 {:?} | 采样 {:.2}s | 目标排队 {:.0}ms | 区间 [{:.2}, {:.2}] Mbit/s",
                names,
                interval,
                p.target_delay_ms,
                p.min_rate_mbps,
                p.max_rate_mbps,
            );
        } else {
            println!("🎚️  [控制环] 未启用（配置里 targets 为空）→ 固定速率模式");
        }

        // 🧮 每帧开销自校准：开销从 0（或存档）起，靠"饱和时段的被动采样"自己学出来
        let cal = if calibrate {
            Some(OverheadCal::from_params(ovh.clone(), calp.clone()))
        } else {
            None
        };
        let saved_rate = cal.as_ref().and_then(|c| c.saved_rate());
        let start_rate = saved_rate.unwrap_or(total);
        let slow_start = calibrate && saved_rate.is_none();
        if calibrate {
            println!(
                "🧮 [自校准] 启用：每帧开销 {:.0}B/帧 | 起始速率 {:.2} Mbit/s {}",
                cal.as_ref().map(|c| c.current).unwrap_or(0.0),
                start_rate * 8.0 / 1e6,
                if slow_start {
                    "（慢启动：先大步爬，撞到排队后转常规步长）"
                } else {
                    "（沿用存档速率）"
                }
            );
        }

        // dry_run=false 启动 ⇒ 立刻把起始 C 落到桶上。
        // 否则桶停在 initial_total_mbps（配置里的 100），而主循环只在"C 变化 >0.5%"时才写桶
        // ⇒ 起始 C 恰好停在上限不动时永远写不进去（实测踩到的坑）。
        if !p.dry_run {
            // 用**夹过的**速率写桶：存档速率可能超出本次配置的区间（实测存档 10.65M、新区间上限 5.4M）
            let sr = start_rate.clamp(
                p.min_rate_mbps * 1_000_000.0 / 8.0,
                p.max_rate_mbps * 1_000_000.0 / 8.0,
            );
            for (bucket, ratio, burst_secs) in &sharers {
                let r = sr * ratio;
                let burst = (r * burst_secs).max(4096.0);
                bucket.set_rate(r, burst);
            }
            println!(
                "🎚️  [控制环] 直接执行：起始 C={:.2} Mbit/s 已落到桶上",
                sr * 8.0 / 1e6
            );
        }

        Self {
            enabled,
            targets,
            buckets: sharers,
            rate: start_rate,
            min_rate: p.min_rate_mbps * 1_000_000.0 / 8.0,
            max_rate: p.max_rate_mbps * 1_000_000.0 / 8.0,
            target_delay_ms: p.target_delay_ms,
            band_ms: p.band_ms,
            up_gain: p.up_gain,
            down_gain: p.down_gain,
            creep_gain: p.creep_gain,
            dry_run: p.dry_run,
            control_period: Duration::from_millis(p.control_ms as u64),
            base_window: Duration::from_secs(p.base_window_s as u64),
            fresh_window: Duration::from_secs_f64((interval * 5.0).max(2.0)),
            last_tick: Instant::now(),
            last_report: Instant::now(),
            starved: false,
            packets_since_tick: 0,
            over_ticks: 0,
            cooldown_until: Instant::now(),
            shared_delay_ms: 0.0,
            current_rate: start_rate,
            last_written: 0.0,
            cal,
            out_pkts: 0,
            out_bytes: 0,
            cal_clock: Instant::now(),
            slow_start,
            pre_cut: None,
            loss_threshold: p.loss_threshold,
            loss_threshold_hard: p.loss_threshold_hard,
            down_gain_hard: p.down_gain_hard,
            last_hard_cut: Instant::now() - Duration::from_secs(60),
            hold_until: Instant::now(),
            hold_after_cut_s: p.hold_after_cut_s,
            fast_until: Instant::now(),
            verdict: "未归因".to_string(),
            win_delay_sum: 0.0,
            win_delay_n: 0,
            out_pkts_tick: 0,
            out_bytes_tick: 0,
        }
    }

    /// SIGHUP 热重载：把配置里"能当场生效"的那几项套上去，返回生效清单。
    /// 不能当场生效的（探测目标要起 ping 进程、桶的组成要重建调度树）由调用方提示重启。
    pub fn apply_params(&mut self, p: &ControlParams) -> Vec<String> {
        let mut done = Vec::new();
        if (self.target_delay_ms - p.target_delay_ms).abs() > 1e-9 {
            self.target_delay_ms = p.target_delay_ms;
            done.push(format!("目标排队 {:.0}ms", p.target_delay_ms));
        }
        if (self.band_ms - p.band_ms).abs() > 1e-9 {
            self.band_ms = p.band_ms;
            done.push(format!("容差带 ±{:.0}ms", p.band_ms));
        }
        if (self.up_gain - p.up_gain).abs() > 1e-9 {
            self.up_gain = p.up_gain;
            done.push(format!("升速步长 ×{:.4}", p.up_gain));
        }
        if (self.down_gain - p.down_gain).abs() > 1e-9 {
            self.down_gain = p.down_gain;
            done.push(format!("降速基准 ×{:.4}", p.down_gain));
        }
        if (self.loss_threshold - p.loss_threshold).abs() > 1e-9 {
            self.loss_threshold = p.loss_threshold;
            done.push(format!("丢包阈值 {:.1}%", p.loss_threshold * 100.0));
        }
        if (self.loss_threshold_hard - p.loss_threshold_hard).abs() > 1e-9 {
            self.loss_threshold_hard = p.loss_threshold_hard;
            done.push(format!("重丢包阈值 {:.1}%", p.loss_threshold_hard * 100.0));
        }
        if (self.down_gain_hard - p.down_gain_hard).abs() > 1e-9 {
            self.down_gain_hard = p.down_gain_hard;
            done.push(format!("重砍系数 ×{:.2}", p.down_gain_hard));
        }
        if (self.creep_gain - p.creep_gain).abs() > 1e-12 {
            self.creep_gain = p.creep_gain;
            done.push(format!("空载慢爬步长 ×{:.5}", p.creep_gain));
        }
        let mn = p.min_rate_mbps * 1_000_000.0 / 8.0;
        let mx = p.max_rate_mbps * 1_000_000.0 / 8.0;
        if (self.min_rate - mn).abs() > 1.0 || (self.max_rate - mx).abs() > 1.0 {
            self.min_rate = mn;
            self.max_rate = mx;
            self.hold_after_cut_s = p.hold_after_cut_s;
            self.rate = self.rate.clamp(mn, mx);
            done.push(format!(
                "速率区间 [{:.2}, {:.2}] Mbit/s",
                p.min_rate_mbps, p.max_rate_mbps
            ));
            // ⚠️ 夹进新区间后必须**立刻重写桶**：主循环只在"C 在本拍内变化 >0.5%"时才写桶，
            // 而这次变化是 apply_params 干的、不经过主循环 ⇒ 不写的话桶会停在旧速率上。
            // （实测：把上限 9→5.4 热重载后，桶仍是旧值，总量仍在 6.5~8.2 Mbit/s ✗）
            if !self.dry_run {
                for (bucket, ratio, burst_secs) in &self.buckets {
                    let r = self.rate * ratio;
                    let burst = (r * burst_secs).max(4096.0);
                    bucket.set_rate(r, burst);
                }
                self.last_written = self.rate;
                done.push(format!("桶已按 C={:.2} Mbit/s 重写", self.rate * 8.0 / 1e6));
            }
        }
        if self.dry_run != p.dry_run {
            self.dry_run = p.dry_run;
            done.push(format!("dry_run={}", p.dry_run));
            // ⚠️ 翻成 false 的那一刻必须**立刻**把 C 落到桶上。
            // 主循环的写桶条件是"C 变化超过 0.5% 才写"（防抖动），而 C 常常正停在上限上不动
            // ⇒ 光翻开关什么都不会发生，桶一直停在 initial_total_mbps（实测踩到的坑）。
            if !self.dry_run {
                for (bucket, ratio, burst_secs) in &self.buckets {
                    let r = self.rate * ratio;
                    let burst = (r * burst_secs).max(4096.0);
                    bucket.set_rate(r, burst);
                self.last_written = self.rate;
                }
                self.last_written = self.rate;
                done.push(format!("C={:.2} Mbit/s 已落到桶上", self.rate * 8.0 / 1e6));
            }
        }
        done
    }

    /// 每轮主循环调一次；内部按 control_period 自我节流，所以调用本身很便宜
    pub fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let since_last = now.duration_since(self.last_tick);
        if since_last < self.control_period {
            return;
        }
        let dt_tick = since_last.as_secs_f64().max(0.001);
        self.last_tick = now;

        let mut fresh = 0usize;
        let mut min_delay = f64::INFINITY;
        let mut rows: Vec<String> = Vec::new();

        for t in &self.targets {
            // 一次拿锁：既算最新样本，也算窗口内的最小值（基线）
            let (latest, base) = {
                let q = t.samples.lock().unwrap();
                let latest = q.back().copied();
                let base = q
                    .iter()
                    .filter(|(at, _)| now.duration_since(*at) <= self.base_window)
                    .map(|(_, r)| *r)
                    .fold(f64::INFINITY, f64::min);
                (latest, base)
            };
            if let Some((at, rtt)) = latest {
                if now.duration_since(at) <= self.fresh_window {
                    fresh += 1;
                    let b = if base.is_finite() { base } else { rtt };
                    let delay = (rtt - b).max(0.0);
                    if delay < min_delay {
                        min_delay = delay;
                    }
                    rows.push(format!("{} {:.0}/{:.0}", t.label, rtt, b));
                }
            }
        }

        let busy = self.packets_since_tick > 0;
        self.packets_since_tick = 0;

        // 这一拍我实际放行了多少（按当前口径换算成"线上字节"）。
        // 用它判断"是不是在逼近自己的限额"：空载/轻载时不许升速 ——
        // 否则 C 会一路飘到上限，把自校准的"饱和"判定全带歪。
        let tick_pps = self.out_pkts_tick as f64 / dt_tick;
        let tick_raw = self.out_bytes_tick as f64 / dt_tick;
        let tick_ovh = self.cal.as_ref().map(|c| c.current).unwrap_or(0.0);
        let tick_accounted = tick_raw + tick_ovh * tick_pps;
        self.out_pkts_tick = 0;
        self.out_bytes_tick = 0;
        let pushing = self.rate > 0.0 && tick_accounted >= 0.7 * self.rate;
        let old = self.rate;
        let mut decision = "保持";
        let rate_before_chain = self.rate; // 丢包总闸用：丢了包就不许升（不管判决链走到哪一步）

        if fresh == 0 {
            // 探针全部失联：冻结当前速率。绝不把"没回应"当成"不拥堵"。
            if !self.starved {
                println!(
                    "⚠️  [控制环] 所有探针失联 → 冻结速率在 {:.2} Mbit/s（不上调）",
                    self.rate * 8.0 / 1e6
                );
                self.starved = true;
            }
            decision = "探针失联·冻结";
        } else {
            self.starved = false;
            self.shared_delay_ms = if min_delay.is_finite() { min_delay } else { 0.0 };
            self.win_delay_sum += self.shared_delay_ms;
            self.win_delay_n += 1;
            let over = self.shared_delay_ms - self.target_delay_ms;
            let cooling = now < self.cooldown_until;

            // ── 归因结算：砍完 1.2 秒回头对账 ──
            // 排队被压下去了 = 这队列确实是我造成的 → 认账，进入快速恢复；
            // 排队没动 = 外面还有人在堵 → 把速率还回去、静默 3 秒，别把自己饿死。
            // （上一版没有这一步，你跑测速把整条线打满时，环路把自己一路砍到 1M 地板。）
            if let Some((was, cut_delay, cut_loss, at)) = self.pre_cut {
                if now.duration_since(at) >= Duration::from_millis(1200) {
                    // 认账判据：排队降了 **或** 丢包降了 ⇒ 都说明这把刀砍对了。
                    // 为什么必须看丢包：上游超限是"静默丢包不给排队"，砍完排队可能纹丝不动，
                    // 只有丢包率会降——只认排队会把正确的砍速冤枉地还回去。
                    // 注意：对账必须看 2 秒短窗口（10s 窗口里全是砍之前的陈年旧账，1.2s 内不可能退掉）。
                    let (wl_now, ua_now) = udp_probe_recent_loss().unwrap_or((0.0, false));
                    let by_loss = cut_loss > self.loss_threshold;
                    let loss_ok = ua_now && by_loss;
                    if self.shared_delay_ms <= cut_delay * 0.75 || loss_ok {
                        // 认账：这队列/丢包是我造成的。**保持**一段时间不升速 —— 让队列真正排空。
                        // （双目标 min 丢包触发的砍速绝不回滚：若 C 从 10M 砍到 8.5M 仍在超 6.5M，
                        //   必须允许它继续往下砍到 6.4M，不能因为第一刀没止住丢包就冤枉还回 10M）
                        self.hold_until = now + Duration::from_secs(self.hold_after_cut_s);
                        self.verdict = if by_loss {
                            format!("丢包是我的({:.0}%→{:.0}%)", cut_loss * 100.0, wl_now * 100.0)
                        } else {
                            "队列是我的".to_string()
                        };
                    } else {
                        println!(
                            "🪞 [控制环] 砍到 {:.2}M 后排队没降（{:.1}ms → {:.1}ms）→ 这队列不是我造成的，速率还回 {:.2} Mbit/s，静默 3s",
                            self.rate * 8.0 / 1e6,
                            cut_delay,
                            self.shared_delay_ms,
                            was * 8.0 / 1e6
                        );
                        self.rate = was;
                        self.hold_until = now + Duration::from_secs(3);
                        self.verdict = "外部队列".to_string();
                    }
                    self.pre_cut = None;
                }
            }
            let holding = now < self.hold_until;

            // 丢包信号：ICMP 被上游豁免，这个 UDP 窗口丢包率是"超发"的硬证据
            let (w_loss, _u_rtt, u_alive) = udp_probe_snapshot().unwrap_or((0.0, 0.0, false));
            let loss_high = u_alive && w_loss > self.loss_threshold;
            let loss_hard = u_alive && w_loss > self.loss_threshold_hard;

            // ── 快降：重丢包 ⇒ 不等观察期，立刻重砍 ──
            // 为什么：丢包信号是"已经丢了很多"的滞后证据，等 2 秒观察期会多挨几百毫秒的丢包。
            let hard_ok = now.duration_since(self.last_hard_cut) >= Duration::from_secs(10);
            if !cooling && loss_hard && hard_ok {
                if self.pre_cut.is_none() {
                    self.pre_cut = Some((self.rate, self.shared_delay_ms, w_loss, now));
                }
                self.rate = (self.rate * self.down_gain_hard).max(self.min_rate);
                self.over_ticks = 0;
                udp_probe_reset_window();
                self.cooldown_until = now + Duration::from_millis(3000);
                self.last_hard_cut = now;
                decision = "丢包·重砍(快降)";
            }
            // 没超标就重新计时（含 target±band 的死区）
            if over <= self.band_ms && !loss_high {
                self.over_ticks = 0;
            }
            if decision == "丢包·重砍(快降)" {
                // 刚重砍过，本拍不再叠加其他降速逻辑
            } else if over > self.band_ms || loss_high {
                // 排队超目标 → 降速，按超标程度加重：只超一点轻拿，超很多直接腰斩。
                // 刚降过就进 500ms 冷却期，否则一个排队事件会被连砍十几刀（上一版的超调就是这么来的）。
                //
                // ⚠️ 单拍超标不算数。第一版改成"看自己饱没饱和"（放行 < 70%×C 就认定不是我），
                // 方向对但太狠：C 高于真实容量时放行量永远够不着 0.7×C ⇒ 一刀都不砍 ⇒
                // C 永远降不下来，反而彻底失去保护。真正的判据是**持续**：
                // 远端突发让排队瞬间冲高又自己退下去（实测把 C 锯在 10~11.6 十几分钟），
                // 而真需要让路的拥塞是持续的 ⇒ 连续 SUSTAIN_TICKS 拍超标才动手。
                self.over_ticks += 1;
                if self.over_ticks < SUSTAIN_TICKS {
                    decision = if over > self.band_ms { "排队超标·观察中" } else { "丢包超标·观察中" };
                } else if holding && !loss_high {
                    decision = "外部队列·保持";
                } else if !cooling || over > self.target_delay_ms * 4.0 {
                    // 记下"砍前多少、砍时排队多长"，1.2 秒后回来对账（见上面的归因结算）
                    if self.pre_cut.is_none() {
                        self.pre_cut = Some((self.rate, self.shared_delay_ms, w_loss, now));
                    }
                    let sev_delay = (over / (self.target_delay_ms * 4.0)).clamp(0.0, 1.0);
                    let sev_loss = ((w_loss - self.loss_threshold) / 0.25).clamp(0.0, 1.0);
                    let sev = sev_delay.max(sev_loss);
                    // 一刀最多砍 28%（旧版最狠砍 50%）：排队冲高时那一刀太狠，
                    // 会把 C 砸到 4M 以下，靠快恢复再爬上来 —— 平均吞吐就少了约 20%。
                    let factor = self.down_gain - (self.down_gain - 0.84) * sev; // 0.92 → 0.84（一刀最多16%，避免从7M直接砸到4M）
                    self.rate *= factor;
                    self.over_ticks = 0; // 砍完清零计数，等下一轮观察窗口
                    udp_probe_reset_window();
                    self.cooldown_until = now + Duration::from_millis(3000);
                    self.last_hard_cut = now;
                    decision = if over > self.band_ms { "排队超标·降速" } else { "丢包超标·降速" };
                    if self.slow_start {
                        // 第一次撞到排队 = 找到瓶颈了 → 慢启动结束，转常规小步长
                        self.slow_start = false;
                        println!(
                            "🧮 [自校准] 慢启动结束（首次撞到排队 @ C={:.2} Mbit/s）→ 转常规步长 ×{:.3}",
                            self.rate * 8.0 / 1e6,
                            self.up_gain
                        );
                    }
                } else {
                    decision = "冷却中";
                }
            } else if over < -self.band_ms {
                // 排队接近 0 = 还没碰到瓶颈，可以升。前提：确实有流量在跑（空载升速毫无意义）。
                if !busy {
                    // 完全空闲也走慢爬：C 被砍低后，空闲期正是悄悄爬回来的时机
                    self.rate *= self.creep_gain;
                    decision = if loss_high { "丢包·保持(不升)" } else { "空载·慢爬" };
                } else if holding {
                    decision = "外部队列·保持";
                } else if !pushing {
                    // 没把自己的限额喂到接近满 → 不能按常规步长升（老版本只看"有没有包"，
                    // 于是轻载时 C 会一路飘到上限，既浪费又把自校准的饱和判定带歪）。
                    // 但完全"不升"会死锁：门槛相对 C 自己算，C 越低越够不着。
                    // 保留一条极慢的上升通道；爬过头了排队会涨，降速逻辑立刻接管。
                    self.rate *= self.creep_gain;
                    decision = "轻载·慢爬";
                } else if self.slow_start {
                    // 慢启动期：还没撞到过排队，可以走得快一点
                    self.rate *= 1.05;
                    decision = "慢启动·大步升";
                } else if now < self.fast_until {
                    // 砍后恢复：降级成常规步长。原来这里 +5%/拍(≈+20%/秒) 会把刚砍下去的
                    // 速率一秒内弹回上限 ⇒ 砍速形同虚设（保持期已经接管，这里只作兜底）
                    self.rate *= self.up_gain;
                    decision = "砍后·缓恢复";
                } else {
                    self.rate *= self.up_gain;
                    decision = "有余量·缓升";
                }
            }
            self.rate = self.rate.clamp(self.min_rate, self.max_rate);
            self.current_rate = self.rate;
        }

        // 只在"自上次写桶以来"的**累计漂移**超过 0.5% 时才真去改桶，避免抖动。
        // ⚠️ 绝不能用"本拍变化 vs 上一拍"：慢爬步长 creep_gain=1.0005（+0.05%/拍）和
        //    缓升 up_gain=1.005（+0.5%/拍）都够不到 0.5% 的阈值 ⇒ 桶永远停在旧速率上。
        //    （实测：C 一路爬到 9.97 Mbit/s，桶还停在启动时的 5.00，下载被卡在 550KB/s ✗）
        let drift = if self.last_written > 0.0 {
            (self.rate - self.last_written).abs() / self.last_written
        } else {
            1.0
        };
        if !self.dry_run && drift > 0.005 {
            let mut wrote = false;
            for (bucket, ratio, burst_secs) in &self.buckets {
                let r = self.rate * ratio;
                let burst = (r * burst_secs).max(4096.0);
                bucket.set_rate(r, burst);
                wrote = true;
            }
            if wrote {
                self.last_written = self.rate;
            }
        }

        // 🧮 每帧开销自校准：这个窗口算不算"饱和且贴着工作点"的样本？
        if let Some(cal) = self.cal.as_mut() {
            let dt = now.duration_since(self.cal_clock).as_secs_f64();
            if dt >= 2.0 {
                let pps = self.out_pkts as f64 / dt;
                let raw_rate = self.out_bytes as f64 / dt;
                let accounted = raw_rate + cal.current * pps; // 按当前口径，我放行了多少
                // 用整个窗口的排队均值判定（旧版用窗口边界那一瞬间的值，排队在跳时全靠运气）
                let mean_delay = if self.win_delay_n > 0 {
                    self.win_delay_sum / self.win_delay_n as f64
                } else {
                    0.0
                };
                let (ok, why) = if pps <= 0.0 {
                    (false, "窗口内一个包都没放行".to_string())
                } else if self.rate <= 0.0 || accounted < 0.3 * self.rate {
                    // 只要求"确实在放东西"（≥30% 限额）。原来要求 ≥90%，本意是"确认线路饱和"，
                    // 但它其实是在要求"我自己就是瓶颈" —— 而样本要的恰恰相反：**线路**是瓶颈。
                    // 小包样本因此被系统性误杀（每帧开销占比大，原文字节速率天然低），
                    // 而解出每帧开销又恰恰离不开小包 —— 90% 那条门槛就是这个死结。
                    // 真正的把关交给：排队落在工作点 + 包速率跨度 + 拟合残差。
                    (
                        false,
                        format!(
                            "放行太少（{:.2} vs 桶允许 {:.2} Mbit/s，占 {:.0}%）→ 没饱和就没有杠杆",
                            accounted * 8.0 / 1e6,
                            self.rate * 8.0 / 1e6,
                            if self.rate > 0.0 {
                                accounted / self.rate * 100.0
                            } else {
                                0.0
                            }
                        ),
                    )
                } else if self.starved {
                    (false, "探针失联，观测不可信".to_string())
                } else if mean_delay < self.target_delay_ms * 0.5
                    || mean_delay > self.target_delay_ms * 4.0
                {
                    (
                        false,
                        format!(
                            "不在工作点（窗口均排队 {:.1}ms，目标 {:.0}ms）→ 说明线没被我自己顶住",
                            mean_delay, self.target_delay_ms
                        ),
                    )
                } else {
                    (true, String::new())
                };
                // 每个窗口都打一行：学不到，就把"为什么学不到"写在脸上。
                // 带时间戳 + 拆出"原文/口径"两部分，便于和壳里并行采样的 tc 计数对账。
                println!(
                    "🧮 [自校准·窗口] {} 放行 {:.2} Mbit/s（原文 {:.2} + 口径 {:.2}）@ {:.0} pps | 窗口均排队 {:.1}ms | 桶允许 {:.2} Mbit/s | {}",
                    chrono::Local::now().format("%H:%M:%S"),
                    accounted * 8.0 / 1e6,
                    raw_rate * 8.0 / 1e6,
                    (cal.current * pps) * 8.0 / 1e6,
                    pps,
                    mean_delay,
                    self.rate * 8.0 / 1e6,
                    if ok {
                        "★ 采到一个样本".to_string()
                    } else {
                        format!("拒：{}", why)
                    }
                );
                cal.note(pps, raw_rate, ok, &why);
                self.out_pkts = 0;
                self.out_bytes = 0;
                self.win_delay_sum = 0.0;
                self.win_delay_n = 0;
                self.cal_clock = now;
            }
            if cal.status_due(Duration::from_secs(10)) {
                match cal.try_solve() {
                    Some((e, c)) => println!(
                        "🧮 [自校准] ★ 解出：每帧开销 {:.0} B/帧，上游真实容量 {:.2} Mbit/s（样本 {} 个，包速率跨度 {:.1}×）",
                        e,
                        c * 8.0 / 1e6,
                        cal.n_samples,
                        cal.spread
                    ),
                    None => println!("🧮 [自校准] {}", cal.status_line()),
                }
                cal.save_state(self.rate);
            }
        }

        // ── 丢包总闸：窗口丢包率高 ⇒ 一律不许升速，把速率摁住 ──
        // 上游超限的证据比排队硬（静默丢包），升速只会让丢得更多。
        if loss_high_now(self.loss_threshold) && self.rate > rate_before_chain {
            self.rate = rate_before_chain;
            decision = "丢包·保持(总闸)";
        }

        if now.duration_since(self.last_report) >= Duration::from_millis(1000) {
            let probe_txt = match udp_probe_snapshot_full() {
                Some((loss, rtt, true, true_loss)) => {
                    format!("稳{:.1}%/真{:.1}%", loss * 100.0, true_loss * 100.0)
                }
                Some(_) => "失联".to_string(),
                None => "-".to_string(),
            };
            println!(
                "🎚️  [控制环]{} C={:.2} Mbit/s | 共享排队 {:.1}ms (目标 {:.0}±{:.0}ms) | {} | {} | {} | 新鲜 {}/{} | {} | 📉 UDP {} | 🧮 {}",
                if self.dry_run { " [只看不做]" } else { "" },
                self.rate * 8.0 / 1e6,
                self.shared_delay_ms,
                self.target_delay_ms,
                self.band_ms,
                decision,
                self.verdict,
                if busy { "有流量" } else { "空闲" },
                fresh,
                self.targets.len(),
                rows.join(" "),
                probe_txt,
                self.cal.as_ref().map(|c| c.short()).unwrap_or_else(|| "-".to_string()),
            );
            self.last_report = now;
        }
    }
}

impl Drop for RttController {
    fn drop(&mut self) {
        for t in &self.targets {
            unsafe {
                libc::kill(t.pid, libc::SIGKILL);
            }
        }
    }
}
