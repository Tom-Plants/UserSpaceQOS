// config.rs —— 把"路径"和"积木"从程序里剥出来，搬进配置文件
//
// 设计口径（都是定过的）：
//   * 一条被整形的路径 = 一对 tun（读 tun_in → 整形 → 写 tun_out）
//     - 出口 tun 决定"这条路往哪个上游走"：注入后由内核按主表路由，
//       所以不同上游 = 不同 tun_out；同一个上游的多条路可以共用同一个 tun_out
//   * 所有路径默认受制于**同一个全局桶**（前提：它们汇合处是同一个物理瓶颈）
//     —— 也就是 [pool] 这一层"积木"：一个 pool = 一份共享预算
//   * 桶的组成（谁吃总量、谁按比例分、burst 的时间常数）全部写在配置里，
//     不再是程序里的常数
//
// 热重载：进程收到 SIGHUP 会重新读这个文件，但**只有一部分能当场生效**
//   （控制环参数：目标排队、带宽区间、上下增益、dry_run…）
//   改路径/改桶的组成需要重启（会明确打在日志里，不悄悄忽略）
use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub control: ControlCfg,
    pub calibration: CalCfg,
    pub pool: Vec<PoolCfg>,
    pub path: Vec<PathCfg>,
    pub leaves: LeavesCfg,
    pub qdisc: std::collections::HashMap<String, crate::qdisc_builder::QdiscNode>,
}

// ---------------- 控制环（原来是一堆 NFQ_* 环境变量） ----------------
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlCfg {
    /// 探测目标列表。**留空 = 控制环完全不启用**（行为退回固定速率）
    pub targets: Vec<String>,
    pub probe_interval_s: f64,
    /// 想维持的"共享排队"时延
    pub target_delay_ms: f64,
    /// 容差带：排队在 target ± band 之内算"刚好"，不做动作
    pub band_ms: f64,
    pub min_rate_mbps: f64,
    pub max_rate_mbps: f64,
    /// 有余量时的升速步长（每拍乘）
    pub up_gain: f64,
    /// 排队超标时的降速基准（每拍乘；另按超标程度加权，最狠到 0.72）
    pub down_gain: f64,
    /// 空载/轻载时的极慢上爬步长（每拍乘）。防"C 被砍低后爬不回来"的死锁。
    pub creep_gain: f64,
    /// 认账后保持不升速的秒数（替代原来的"快恢复"，让砍速站得住）
    pub hold_after_cut_s: u64,
    /// UDP 丢包探针的目标（对端需跑 qos_udp_echo.py 回声器）。
    /// 空 = 不启用。为什么单列：ICMP 被上游豁免，UDP 才跟大流量同命运。
    pub udp_probe_targets: Vec<String>,
    /// 回声器端口
    pub udp_probe_port: u16,
    /// 探针间隔（毫秒）
    pub udp_probe_interval_ms: u64,
    /// 窗口丢包率超过它 ⇒ 认定上游超限，当"排队超标"一样砍速
    pub loss_threshold: f64,
    /// 重丢包阈值：超过它 ⇒ 不等观察期，立刻重砍（快降）
    pub loss_threshold_hard: f64,
    /// 重砍系数
    pub down_gain_hard: f64,
    /// UDP 探针微突发：每 burst_ms 打 burst_pkts 个 pkt_bytes 大小的包（0 = 关）
    pub udp_probe_burst_pkts: usize,
    pub udp_probe_burst_ms: u64,
    pub udp_probe_pkt_bytes: usize,
    pub control_ms: f64,
    /// RTT 基线的观察窗（取窗口内最小值当基线）
    pub base_window_s: f64,
    /// 只观测不执行：照常算 C 并打日志，但不真去改桶
    pub dry_run: bool,
}

impl Default for ControlCfg {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            probe_interval_s: 0.2,
            target_delay_ms: 10.0,
            band_ms: 3.0,
            min_rate_mbps: 1.0,
            max_rate_mbps: 24.0,
            up_gain: 1.005,
            down_gain: 0.92,
            creep_gain: 1.0005,
            hold_after_cut_s: 6,
            udp_probe_targets: vec![],
            udp_probe_port: 15390,
            udp_probe_interval_ms: 200,
            loss_threshold: 0.05,
            loss_threshold_hard: 0.08,
            down_gain_hard: 0.6,
            udp_probe_burst_pkts: 0,
            udp_probe_burst_ms: 2000,
            udp_probe_pkt_bytes: 800,
            control_ms: 250.0,
            base_window_s: 600.0,
            dry_run: false,
        }
    }
}

// ---------------- 每帧开销自校准 ----------------
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CalCfg {
    /// 只有 TUN 版用得上（它只有一条路径、一个口径）
    pub enabled: bool,
    pub state_file: String,
    /// 样本保留时长：上游容量会漂，旧样本要按时间去旧
    pub sample_window_s: u64,
    pub min_samples: usize,
    /// 包速率跨度下限（max/min）：太小则"斜率"和"截距"在数学上分不开
    pub min_spread: f64,
    pub max_overhead: f64,
}

impl Default for CalCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            state_file: "/var/lib/qos_shaper/state".to_string(),
            sample_window_s: 120,
            min_samples: 8,
            min_spread: 3.0,
            max_overhead: 2000.0,
        }
    }
}

// ---------------- 叶子（队列）参数：TTL 预算 + 内存界 ----------------
// 「TTL = 0」表示**不挂** TtlDropWrapper：
//   * 高优/交互流：要 TTL（宁可丢包也要保延迟）
//   * 低优批量流（下载）：不要 TTL —— TCP 自己用丢包做拥塞控制，
//     给它"延迟预算"反而会在它排队不到一个包的时间就把包丢掉 ⇒ 饿死。
//    只留 HeadDropFifo 的包数上限当内存界。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LeavesCfg {
    /// 高优短叶（class 2/3 之外走短叶的那些，单位 ms；0 = 不挂 TTL）
    pub high_ttl_ms: u64,
    pub high_fifo_pkts: usize,
    /// 高优长叶（class 3，批量但高优）
    pub high_long_ttl_ms: u64,
    /// 低优批量叶（class 5 的批量流 —— **下载走这条**）
    pub low_bulk_ttl_ms: u64,
    /// ★ 低优最多积压多少包。这是"下载最多排多久"的唯一旋钮：
    ///   积压秒数 ≈ fifo_pkts × 1500B ÷ 低优速率
    pub low_bulk_fifo_pkts: usize,
    /// 低优稀疏叶（小流/交互）
    pub low_sparse_ttl_ms: u64,
    pub low_sparse_fifo_pkts: usize,
}

impl Default for LeavesCfg {
    fn default() -> Self {
        Self {
            high_ttl_ms: 200,
            high_fifo_pkts: 2048,
            high_long_ttl_ms: 100,
            low_bulk_ttl_ms: 0, // 默认不挂 TTL（低优=下载=TCP，让它自己管）
            low_bulk_fifo_pkts: 32,
            low_sparse_ttl_ms: 200,
            low_sparse_fifo_pkts: 2048,
        }
    }
}

// ---------------- 积木：一个 pool = 一份共享预算 ----------------
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PoolCfg {
    pub name: String,
    /// 初始总量（Mbit/s）。控制环启用时它会被接管；不启用时就是固定速率
    #[serde(default = "default_initial_total")]
    pub initial_total_mbps: f64,
    /// 组成这块积木的桶：恰好一个 role="total"，其余 role="share"
    pub bucket: Vec<BucketCfg>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BucketCfg {
    pub name: String,
    /// "total" —— 直接吃总量；"share" —— 按 share 比例分总量
    pub role: String,
    #[serde(default)]
    pub share: f64,
    /// burst = rate × tau（时间常数），这是"兜里能攒多少"的来源
    #[serde(default = "default_tau_ms")]
    pub tau_ms: f64,
}

// ---------------- 路径：一对 tun = 一类流量 ----------------
#[derive(Debug, Clone, Deserialize)]
pub struct PathCfg {
    pub name: String,
    pub tun_in: String,
    pub tun_out: String,
    /// 分类号，沿用原来 iptables 队列号的语义：
    ///   2 = 高优短叶（10ms 队列）  3 = 高优长叶（100ms + ACK 过滤）
    ///   0/1/4/5 = 默认叶
    pub class: usize,
    #[serde(default = "default_pool_name")]
    pub pool: String,
}

fn default_initial_total() -> f64 {
    6.9
}
fn default_tau_ms() -> f64 {
    200.0
}
fn default_pool_name() -> String {
    "default".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            control: ControlCfg::default(),
            calibration: CalCfg::default(),
            pool: Vec::new(),
            path: Vec::new(),
            leaves: LeavesCfg::default(),
            qdisc: std::collections::HashMap::new(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Config, String> {
        let txt = fs::read_to_string(path)
            .map_err(|e| format!("读不到配置文件 {}：{}", path, e))?;
        let cfg: Config = toml::from_str(&txt)
            .map_err(|e| format!("配置文件 {} 解析失败：{}", path, e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验：把"不支持的写法"当场骂出来，别让它悄悄跑起来
    fn validate(&self) -> Result<(), String> {
        if self.pool.is_empty() {
            return Err("配置里至少要有一个 [[pool]]（一块共享预算）".to_string());
        }
        if self.pool.len() > 1 {
            return Err(format!(
                "现在只支持一个 [[pool]]（多条路受制于同一个物理瓶颈）；配置里有 {} 个。\n\
                 要支持「不同瓶颈各一条环路」需要把控制环也做成每池一份，那是下一步。",
                self.pool.len()
            ));
        }
        let mut bucket_names = std::collections::HashSet::new();
        for p in &self.pool {
            let totals = p.bucket.iter().filter(|b| b.role == "total").count();
            if totals != 1 {
                return Err(format!(
                    "pool \"{}\" 里必须有且只有一个 role=\"total\" 的桶（现在 {} 个）",
                    p.name, totals
                ));
            }
            for b in &p.bucket {
                if !bucket_names.insert(b.name.clone()) {
                    return Err(format!(
                        "pool \"{}\" 里有重名的桶 \"{}\"",
                        p.name, b.name
                    ));
                }
                if b.role != "total" && b.role != "share" {
                    return Err(format!(
                        "pool \"{}\" 的桶 \"{}\" role=\"{}\" 不认识（只能写 total / share）",
                        p.name, b.name, b.role
                    ));
                }
                if b.role == "share" && !(b.share > 0.0 && b.share <= 1.0) {
                    return Err(format!(
                        "pool \"{}\" 的桶 \"{}\" 的 share 要落在 (0,1] 里，现在是 {}",
                        p.name, b.name, b.share
                    ));
                }
            }
        }
        crate::qdisc_builder::validate_tree(&self.qdisc, &bucket_names)?;
        if self.path.is_empty() {
            return Err("配置里至少要有一条 [[path]]（一对 tun）".to_string());
        }
        for p in &self.path {
            if p.tun_in == p.tun_out {
                return Err(format!(
                    "路径 \"{}\" 的 tun_in 和 tun_out 不能是同一个设备（会自己读自己，无限打转）",
                    p.name
                ));
            }
            if !self.pool.iter().any(|q| q.name == p.pool) {
                return Err(format!(
                    "路径 \"{}\" 指向 pool \"{}\"，但配置里没有这个 pool",
                    p.name, p.pool
                ));
            }
        }
        Ok(())
    }

    pub fn pool(&self, name: &str) -> &PoolCfg {
        self.pool
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("pool {} 不存在（validate 应该早就拦住了）", name))
    }
}
