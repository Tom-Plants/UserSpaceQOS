// overhead_cal.rs —— 每帧开销的自校准（被动采样，零额外流量）
//
// 一句话原理：
//     饱和时  线上真实速率 = 我数到的原文字节速率 + 每帧开销 × 包速率
//     即      R_raw = C − E × pps
//   把 R_raw 对 pps 画一条直线：截距 = 上游真实容量 C，斜率取负 = 每帧开销 E。
//
// 直观版（装车）：
//   两次都把车装满——第一次全装大箱子，第二次全装小盒子。
//   小盒子那轮你数到的"货"少掉了一大截，而车装的总量是一样的；
//   少掉的货 ÷ 多出来的箱子数 = 每个箱子底下那层你看不见的托盘的厚度。
//
// 纪律（都是数值模拟算出来的教训）：
//   ① 只在"饱和且贴着工作点"时采样 —— 没把线跑满就没有杠杆，什么都学不到
//   ② 样本的包速率跨度要够（max/min ≥ 3×）—— 否则"斜率"和"截距"在数学上分不开
//   ③ 采样窗口不能太长（上游容量会漂；漂 30% 会把 E 算歪 40%）→ 样本按时间去旧
//   ④ 解不出来就不动，沿用当前值 —— 宁可保守，不瞎猜

use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct OverheadCal {
    ovh: Arc<AtomicUsize>,
    /// 样本：(包速率 pps, 原文字节速率 B/s, 采样时刻)
    samples: VecDeque<(f64, f64, Instant)>,
    /// 当前生效的每帧开销（字节/帧）
    pub current: f64,
    /// 上次解出的上游真实容量（字节/秒）—— 副产品，你以前完全不知道的那个数
    pub est_c: f64,
    pub n_samples: usize,
    pub spread: f64,
    pub last_status: Instant,
    last_solve: Instant,
    last_reject: String,
    /// 最近一个采样窗口被拒的原因（和解算失败的原因分开记，不然互相盖掉，日志里看不到真相）
    win_reject: String,
    state_path: String,
    min_samples: usize,
    min_spread: f64,
    sample_window: Duration,
    max_overhead: f64,
    saved_rate: Option<f64>,
}

/// 自校准的可调参数（来自配置文件 [calibration] 段；热重载不覆盖，改它们要重启）
#[derive(Debug, Clone)]
pub struct CalParams {
    pub state_file: String,
    pub sample_window_s: u64,
    pub min_samples: usize,
    pub min_spread: f64,
    pub max_overhead: f64,
}

impl Default for CalParams {
    fn default() -> Self {
        Self {
            state_file: "/var/lib/qos_shaper/state".to_string(),
            sample_window_s: 120,
            min_samples: 8,
            min_spread: 3.0,
            max_overhead: 2000.0,
        }
    }
}

impl OverheadCal {
    pub fn from_params(ovh: Arc<AtomicUsize>, p: CalParams) -> Self {
        let state_path = p.state_file.clone();
        let (mut e, mut rate) = (0.0f64, None::<f64>);
        match fs::read_to_string(&state_path) {
            Ok(txt) => {
                for line in txt.lines() {
                    let line = line.trim();
                    if let Some(v) = line.strip_prefix("overhead=") {
                        if let Ok(x) = v.trim().parse::<f64>() {
                            if (0.0..=5000.0).contains(&x) {
                                e = x;
                            }
                        }
                    } else if let Some(v) = line.strip_prefix("rate=") {
                        if let Ok(x) = v.trim().parse::<f64>() {
                            if x > 1000.0 {
                                rate = Some(x);
                            }
                        }
                    }
                }
                println!(
                    "🧮 [自校准] 读回存档 {}：每帧开销 {:.0} B/帧，上次收敛速率 {}",
                    state_path,
                    e,
                    rate.map(|r| format!("{:.2} Mbit/s", r * 8.0 / 1e6))
                        .unwrap_or_else(|| "无".to_string())
                );
            }
            Err(_) => println!("🧮 [自校准] 没有存档（{}）→ 每帧开销从 0 开始学", state_path),
        }
        ovh.store(e.round() as usize, Ordering::Relaxed);
        Self {
            ovh,
            samples: VecDeque::new(),
            current: e,
            est_c: 0.0,
            n_samples: 0,
            spread: 0.0,
            last_status: Instant::now(),
            last_solve: Instant::now() - Duration::from_secs(3600),
            last_reject: "还没采到样".to_string(),
            win_reject: "还没采到样".to_string(),
            state_path,
            min_samples: p.min_samples,
            min_spread: p.min_spread,
            sample_window: Duration::from_secs(p.sample_window_s),
            max_overhead: p.max_overhead,
            saved_rate: rate,
        }
    }

    /// 存档里上次收敛的速率（有的话直接从那儿起步，不用重新学）
    pub fn saved_rate(&self) -> Option<f64> {
        self.saved_rate
    }

    /// 每个采样窗口（约 2 秒）调一次。ok=false 只记原因，方便在日志里看到"为什么学不到"。
    pub fn note(&mut self, pps: f64, raw_rate: f64, ok: bool, why: &str) {
        let now = Instant::now();
        self.samples
            .retain(|(_, _, at)| now.duration_since(*at) <= self.sample_window);
        if !ok {
            self.win_reject = why.to_string();
            self.n_samples = self.samples.len();
            return;
        }
        self.samples.push_back((pps, raw_rate, now));
        let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
        for (p, _, _) in &self.samples {
            if *p < lo {
                lo = *p;
            }
            if *p > hi {
                hi = *p;
            }
        }
        self.spread = if lo.is_finite() && lo > 0.0 { hi / lo } else { 0.0 };
        self.n_samples = self.samples.len();
        self.win_reject.clear();
    }

    /// 试着解一次。返回 Some((每帧开销, 上游容量)) 表示这次真的有更新、且已经写进原子变量。
    pub fn try_solve(&mut self) -> Option<(f64, f64)> {
        if self.samples.len() < self.min_samples {
            self.last_reject = format!("样本还不够（{} / {}）", self.samples.len(), self.min_samples);
            return None;
        }
        if self.spread < self.min_spread {
            self.last_reject = format!(
                "包速率跨度只有 {:.1}×（要 ≥{:.0}× 才解得开）",
                self.spread, self.min_spread
            );
            return None;
        }
        // 最小二乘拟合 R_raw = a + b·pps
        let n = self.samples.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (p, r, _) in &self.samples {
            sx += p;
            sy += r;
            sxx += p * p;
            sxy += p * r;
        }
        let den = n * sxx - sx * sx;
        if den.abs() < 1e-9 {
            self.last_reject = "样本挤在一起，解不开".to_string();
            return None;
        }
        let b = (n * sxy - sx * sy) / den; // 斜率 = −E
        let a = (sy - b * sx) / n; // 截距 = C
        let (e, c) = (-b, a);

        // 残差（相对 RMS）
        let (mut ss, mut tt) = (0.0f64, 0.0f64);
        for (p, r, _) in &self.samples {
            let pred = a + b * p;
            ss += (r - pred) * (r - pred);
            tt += r * r;
        }
        let rel_res = if tt > 0.0 { (ss / tt).sqrt() } else { 1.0 };

        if !e.is_finite() || !c.is_finite() || e < 0.0 || e > self.max_overhead || c <= 0.0 {
            self.last_reject = format!(
                "解出来的数不合理（E={:.0}B/帧，C={:.2} Mbit/s）",
                e,
                c * 8.0 / 1e6
            );
            return None;
        }
        if rel_res > 0.25 {
            self.last_reject = format!("拟合残差 {:.0}% 太大（上游容量可能在漂）", rel_res * 100.0);
            return None;
        }
        if Instant::now().duration_since(self.last_solve) < Duration::from_secs(30) {
            self.last_reject = "距上次更新不到 30 秒，先不抖".to_string();
            return None;
        }
        if self.current > 0.0 && (e - self.current).abs() < 8.0 {
            self.last_reject = format!("和现值（{:.0}B）差不多，不动", self.current);
            return None;
        }
        self.last_solve = Instant::now();
        self.current = e;
        self.est_c = c;
        self.ovh.store(e.round() as usize, Ordering::Relaxed);
        Some((e, c))
    }

    /// 长版状态（每 10 秒打一次，便于事后分析为什么学不到）
    pub fn status_line(&self) -> String {
        format!(
            "口径 {:.0}B/帧 | 样本 {} 跨度 {:.1}× | 解得上游 {:.2} Mbit/s | 解算:{} | 最近窗口:{}",
            self.current,
            self.samples.len(),
            self.spread,
            self.est_c * 8.0 / 1e6,
            if self.last_reject.is_empty() {
                "待解".to_string()
            } else {
                self.last_reject.clone()
            },
            self.win_reject.clone()
        )
    }

    /// 短版（挂在每秒面板行尾）
    pub fn short(&self) -> String {
        format!("口径{:.0}B/帧 样本{}", self.current, self.samples.len())
    }

    pub fn status_due(&mut self, every: Duration) -> bool {
        if Instant::now().duration_since(self.last_status) >= every {
            self.last_status = Instant::now();
            true
        } else {
            false
        }
    }

    /// 落盘：每帧开销 + 当前收敛速率（重启直接从这两个数接着跑）
    pub fn save_state(&self, rate: f64) {
        let body = format!("overhead={:.0}\nrate={:.0}\n", self.current, rate);
        let _ = fs::write(&self.state_path, body);
    }
}
