//! UDP 丢包探针（多目标取最小值 min）
//!
//! 为什么需要它：上游在超限时"直接丢包、不给排队延迟"，而 ICMP 探针被豁免
//! （实测：同样的拥塞下 ICMP 丢 0%，UDP 丢 66%）。所以只看 RTT 的控制环
//! 对"超发"完全失明——实测上行硬顶 ≈6.5Mbps，8M 流量静默丢 20% 而 RTT 纹丝不动。
//!
//! 为什么必须多目标取 min（与 RTT 探针同逻辑）：
//! 单打 10.11.13.3（NAS）时，一旦开启 NAS 下载，13.3 自己的上行先丢 ~10% 的包，
//! 导致回声死在 13.3→hub 的入向路上，骗得 hub 误以为是自己 7M 出向满了而猛砍 C。
//! 同时打 10.11.12.6 和 10.11.13.3 取 min：
//!   - 仅某条腿自己抖（如 13.3 上传丢包、12.6 为 0%）⇒ min = 0%，不误砍；
//!   - hub 自己的 7M 出向真被打爆 ⇒ 所有出向 UDP 一起挨刀，两边同时丢 ⇒ min > 阈值，立刻砍 C。

use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 滑动窗口采样点：(时刻ms, 累计发送, 累计收回)
pub type Hist = Arc<std::sync::Mutex<std::collections::VecDeque<(u64, u64, u64)>>>;

pub struct TargetProbe {
    pub target: String,
    pub sent: AtomicU64,
    pub recv: AtomicU64,
    pub sent_s: AtomicU64,
    pub recv_s: AtomicU64,
    pub last_ok_ms: AtomicU64,
    pub hist: Hist,
    pub hist_b: Hist,
}

impl TargetProbe {
    fn new(target: String) -> Self {
        Self {
            target,
            sent: AtomicU64::new(0),
            recv: AtomicU64::new(0),
            sent_s: AtomicU64::new(0),
            recv_s: AtomicU64::new(0),
            last_ok_ms: AtomicU64::new(0),
            hist: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            hist_b: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        }
    }

    pub fn alive(&self) -> bool {
        let la = self.last_ok_ms.load(Ordering::Relaxed);
        la > 0 && now_ms().saturating_sub(la) < 3000
    }

    pub fn window_loss(&self, window_ms: u64) -> f64 {
        Self::win_from(&self.hist, window_ms)
    }

    pub fn window_loss_total(&self, window_ms: u64) -> f64 {
        Self::win_from(&self.hist_b, window_ms)
    }

    fn win_from(hist: &Hist, window_ms: u64) -> f64 {
        let now = now_ms();
        let g = match hist.lock() {
            Ok(g) => g,
            Err(_) => return 0.0,
        };
        let &( _, s_now, r_now) = match g.back() {
            Some(x) => x,
            None => return 0.0,
        };
        let mut base: Option<(u64, u64)> = None;
        for (t, st, rc) in g.iter() {
            if now.saturating_sub(*t) <= window_ms {
                base = Some((*st, *rc));
                break;
            }
        }
        let (s0, r0) = base.unwrap_or((s_now, r_now.min(s_now)));
        let ds = s_now.saturating_sub(s0);
        if ds < 40 {
            return 0.0;
        }
        let dr = r_now.saturating_sub(r0).min(ds);
        1.0 - dr as f64 / ds as f64
    }
}

#[derive(Clone)]
pub struct UdpProbeHandles {
    pub targets: Arc<Vec<TargetProbe>>,
    pub rtt_us: Arc<AtomicU64>,
}

impl UdpProbeHandles {
    /// 至少有一个回声器存活
    pub fn alive(&self) -> bool {
        self.targets.iter().any(|t| t.alive())
    }

    pub fn rtt_ms(&self) -> f64 {
        self.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// 共享稳态窗口丢包率（各存活目标取 min）—— **控制环用这个**
    pub fn window_loss(&self, window_ms: u64) -> f64 {
        let mut min_loss = f64::INFINITY;
        for t in self.targets.iter() {
            if t.alive() {
                let l = t.window_loss(window_ms);
                if l < min_loss {
                    min_loss = l;
                }
            }
        }
        if min_loss.is_finite() { min_loss } else { 0.0 }
    }

    /// 共享总窗口丢包率（含突发，各存活目标取 min）—— 仪表盘用
    pub fn window_loss_total(&self, window_ms: u64) -> f64 {
        let mut min_loss = f64::INFINITY;
        for t in self.targets.iter() {
            if t.alive() {
                let l = t.window_loss_total(window_ms);
                if l < min_loss {
                    min_loss = l;
                }
            }
        }
        if min_loss.is_finite() { min_loss } else { 0.0 }
    }

    /// 砍速后清空旧窗口账本（"一罪不二罚"：砍之前的旧丢包不能拿来对砍之后的新速率再判第二次刑）
    pub fn reset_window(&self) {
        for t in self.targets.iter() {
            if let Ok(mut g) = t.hist.lock() {
                g.clear();
            }
            if let Ok(mut g) = t.hist_b.lock() {
                g.clear();
            }
        }
    }
}

/// 起常驻线程：每 interval_ms 向每个 target:port 发一个 QOSPROBE，并实时捞回射。
pub fn spawn(
    targets: Vec<String>,
    port: u16,
    interval_ms: u64,
    burst_pkts: usize,
    burst_ms: u64,
    pkt_bytes: usize,
) -> UdpProbeHandles {
    let t_list: Vec<TargetProbe> = targets.into_iter().map(TargetProbe::new).collect();
    let h = UdpProbeHandles {
        targets: Arc::new(t_list),
        rtt_us: Arc::new(AtomicU64::new(0)),
    };
    let hh = h.clone();
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                println!("📉 [UDP探针] 绑定失败: {e}");
                return;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(10)));
        let mut buf = [0u8; 2048];
        let mut seq: u64 = 0;
        let mut inflight: std::collections::HashMap<u64, Instant> =
            std::collections::HashMap::new();
        let mut last_burst = Instant::now();
        let pad_len = pkt_bytes.saturating_sub(24).max(8);
        let pad: String = "x".repeat(pad_len);
        let step = Duration::from_millis(interval_ms.max(50));

        loop {
            let tick_start = Instant::now();

            // 发：稳态探针
            for tp in hh.targets.iter() {
                seq = seq.wrapping_add(1);
                let msg = format!("QOSPROBE {} {}", seq, now_ms());
                if sock.send_to(msg.as_bytes(), (tp.target.as_str(), port)).is_ok() {
                    tp.sent.fetch_add(1, Ordering::Relaxed);
                    tp.sent_s.fetch_add(1, Ordering::Relaxed);
                    inflight.insert(seq, Instant::now());
                }
            }

            // 微突发：每 burst_ms 打一串大包
            if burst_pkts > 0 && last_burst.elapsed() >= Duration::from_millis(burst_ms.max(200)) {
                for _ in 0..burst_pkts {
                    seq = seq.wrapping_add(1);
                    let msg = format!("QOSBURST {} {} {}", seq, now_ms(), pad);
                    for tp in hh.targets.iter() {
                        if sock.send_to(msg.as_bytes(), (tp.target.as_str(), port)).is_ok() {
                            tp.sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                last_burst = Instant::now();
            }

            if inflight.len() > 4096 {
                inflight.clear();
            }

            // 在整个 step 周期内持续收包（等本轮包飞回后再落快照，绝不把在途包误算成丢包）
            while tick_start.elapsed() < step {
                match sock.recv_from(&mut buf) {
                    Ok((n, peer)) => {
                        let peer_ip = peer.ip().to_string();
                        let txt = String::from_utf8_lossy(&buf[..n]);
                        if let Some(rest) = txt.strip_prefix("QOSPROBE ") {
                            if let Some(tp) = hh.targets.iter().find(|t| t.target == peer_ip) {
                                tp.recv.fetch_add(1, Ordering::Relaxed);
                                tp.recv_s.fetch_add(1, Ordering::Relaxed);
                                tp.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                            }
                            let mut it = rest.split_whitespace();
                            if let Some(sq) = it.next().and_then(|x| x.parse::<u64>().ok()) {
                                if let Some(t0) = inflight.remove(&sq) {
                                    hh.rtt_us
                                        .store(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
                                }
                            }
                        } else if txt.starts_with("QOSBURST ") {
                            if let Some(tp) = hh.targets.iter().find(|t| t.target == peer_ip) {
                                tp.recv.fetch_add(1, Ordering::Relaxed);
                                tp.last_ok_ms.store(now_ms(), Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => {
                        // read_timeout (10ms) 到了，继续循环直到 tick_start.elapsed() >= step
                    }
                }
            }

            // 收完本轮回声后再记滑动窗口快照（此时距发包已过 200ms >> RTT，在途包已归零）
            let tnow = now_ms();
            let cutoff = tnow.saturating_sub(30_000);
            for tp in hh.targets.iter() {
                if let Ok(mut g) = tp.hist.lock() {
                    g.push_back((
                        tnow,
                        tp.sent_s.load(Ordering::Relaxed),
                        tp.recv_s.load(Ordering::Relaxed),
                    ));
                    while g.front().map(|(t, _, _)| *t < cutoff).unwrap_or(false) {
                        g.pop_front();
                    }
                }
                if let Ok(mut g) = tp.hist_b.lock() {
                    g.push_back((
                        tnow,
                        tp.sent.load(Ordering::Relaxed),
                        tp.recv.load(Ordering::Relaxed),
                    ));
                    while g.front().map(|(t, _, _)| *t < cutoff).unwrap_or(false) {
                        g.pop_front();
                    }
                }
            }
        }
    });
    h
}
