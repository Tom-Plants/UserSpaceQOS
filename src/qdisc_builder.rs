//! qdisc_builder.rs —— 配置文件驱动的可递归嵌套 Qdisc 调度树拼装器
//!
//! 设计目标：
//!   1. 所有 Qdisc 积木（叶子 `tail_drop`/`head_drop`、包装器 `ttl_drop`/`ack_filter`/`rate_limit`、
//!      调度器 `htb`/`prio`/`dual_fair`/`sparse`/`drr`）均可在 `qos.toml` 的 `[qdisc.<节点名>]` 中自由组合与无限层嵌套。
//!   2. 子节点既支持按名字引用（`child = "my_leaf"`），也支持直接内联表（`child = { type = "tail_drop", limit = 300 }`）。
//!   3. 若配置文件未定义 `[qdisc.root]`，启动时明确打印警告，并使用默认树：`TailDropFifo(limit=300)`（挂在全局桶下限速）。

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use crate::{
    five_tuple::FiveTuple,
    nfq_message::NfqMessage as Message,
    packet_context::PacketContext,
    qdisc::{
        leaf::{HeadDropFifo, TailDropFifo},
        scheduler::{ClassDrrQdisc, DualFairQdisc, HtbQdisc, PrioQdisc, SparseQdisc},
        wrapper::{RateLimitQdisc, TcpAckFilterQdisc, TtlDropWrapper},
        Qdisc,
    },
    token_bucket::{SharedBucket, TokenBucketLimiter},
};

pub type BoxQdisc = Box<dyn Qdisc<Message, FiveTuple>>;

/// 子节点引用：既可以写名字 `"short_leaf"`，也可以直接内联 `{ type = "tail_drop", limit = 300 }`
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum QdiscRef {
    Name(String),
    Inline(Box<QdiscNode>),
}

/// 单个 Qdisc 积木节点的通用配置结构
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct QdiscNode {
    /// 节点类型：
    ///   叶子：  "tail_drop" (别名 "fifo" / "tail_drop_fifo") | "head_drop" (别名 "head_drop_fifo")
    ///   包装器："ttl_drop" | "ack_filter" (别名 "tcp_ack_filter") | "rate_limit"
    ///   调度器："htb" | "prio" | "dual_fair" | "sparse" | "drr" (别名 "class_drr")
    #[serde(rename = "type")]
    pub kind: String,

    // ── 叶子参数 ──
    /// 队列最大包数（默认 300）
    pub limit: Option<usize>,

    // ── 通用包装语法糖（任意节点都可直接附带，也可单独写成 type="ttl_drop" / "ack_filter"）──
    /// 若设置且 > 0，在该节点外层包一层 TtlDropWrapper(ttl_ms)
    pub ttl_ms: Option<u64>,
    /// 若为 true，在该节点外层包一层 TcpAckFilterQdisc
    pub ack_filter: bool,

    // ── 单子节点（用于 ttl_drop / ack_filter / rate_limit / drr）──
    pub child: Option<QdiscRef>,

    // ── rate_limit 参数 ──
    pub bucket: Option<String>,
    pub reserve_bucket: Option<String>,

    // ── htb / prio 参数 ──
    pub global_bucket: Option<String>,
    pub high_bucket: Option<String>,
    pub low_bucket: Option<String>,
    pub high_classes: Option<Vec<usize>>,
    pub high: Option<QdiscRef>,
    pub low: Option<QdiscRef>,

    // ── dual_fair 参数 ──
    pub a_classes: Option<Vec<usize>>,
    pub a: Option<QdiscRef>,
    pub b: Option<QdiscRef>,

    // ── sparse 参数 ──
    pub sparse: Option<QdiscRef>,
    pub bulk: Option<QdiscRef>,

    // ── drr / dual_fair 参数 ──
    /// DRR 分流维度："flow" (五元组) | "src_ip" | "dst_ip" | "class" | "src_or_dst_by_class"
    pub by: Option<String>,
    pub quantum: Option<i32>,
}

/// 启动期静态校验整棵 `[qdisc]` 配置树（缺字段、引用不存在的节点/桶、循环引用当场报错）
pub fn validate_tree(
    nodes: &HashMap<String, QdiscNode>,
    bucket_names: &HashSet<String>,
) -> Result<(), String> {
    if nodes.is_empty() || !nodes.contains_key("root") {
        // 允许不写 [qdisc.root]，运行时会告警并回退到 TailDropFifo(300)
        return Ok(());
    }
    let mut visiting = HashSet::new();
    validate_ref(
        &QdiscRef::Name("root".to_string()),
        nodes,
        bucket_names,
        &mut visiting,
        "root",
    )
}

fn validate_ref(
    r: &QdiscRef,
    nodes: &HashMap<String, QdiscNode>,
    bucket_names: &HashSet<String>,
    visiting: &mut HashSet<String>,
    path: &str,
) -> Result<(), String> {
    match r {
        QdiscRef::Name(name) => {
            if !visiting.insert(name.clone()) {
                return Err(format!(
                    "[qdisc] 存在循环引用：{} -> {}（节点不能自己套自己）",
                    path, name
                ));
            }
            let node = nodes
                .get(name)
                .ok_or_else(|| format!("[qdisc] 路径 {} 引用了不存在的节点 [qdisc.{}]", path, name))?;
            let sub_path = format!("{}.{}", path, name);
            validate_node(node, nodes, bucket_names, visiting, &sub_path)?;
            visiting.remove(name);
            Ok(())
        }
        QdiscRef::Inline(node) => validate_node(node, nodes, bucket_names, visiting, path),
    }
}

fn require_ref<'a>(
    opt: &'a Option<QdiscRef>,
    field: &str,
    path: &str,
) -> Result<&'a QdiscRef, String> {
    opt.as_ref()
        .ok_or_else(|| format!("[qdisc] 节点 {} 缺少必填子节点字段 `{}`", path, field))
}

fn require_bucket(
    opt: &Option<String>,
    field: &str,
    path: &str,
    bucket_names: &HashSet<String>,
) -> Result<(), String> {
    let b = opt
        .as_ref()
        .ok_or_else(|| format!("[qdisc] 节点 {} 缺少必填桶字段 `{}`", path, field))?;
    if !bucket_names.contains(b) {
        return Err(format!(
            "[qdisc] 节点 {} 的 `{}` 引用了不存在的桶 \"{}\"（请检查 [[pool.bucket]]）",
            path, field, b
        ));
    }
    Ok(())
}

fn validate_node(
    node: &QdiscNode,
    nodes: &HashMap<String, QdiscNode>,
    bucket_names: &HashSet<String>,
    visiting: &mut HashSet<String>,
    path: &str,
) -> Result<(), String> {
    let k = node.kind.trim().to_lowercase();
    match k.as_str() {
        "tail_drop" | "tail_drop_fifo" | "fifo" | "head_drop" | "head_drop_fifo" => {
            if let Some(lim) = node.limit {
                if lim == 0 {
                    return Err(format!("[qdisc] 叶子节点 {} 的 limit 不能为 0", path));
                }
            }
            Ok(())
        }
        "ttl_drop" => {
            let c = require_ref(&node.child, "child", path)?;
            validate_ref(c, nodes, bucket_names, visiting, &format!("{}.child", path))
        }
        "ack_filter" | "tcp_ack_filter" => {
            let c = require_ref(&node.child, "child", path)?;
            validate_ref(c, nodes, bucket_names, visiting, &format!("{}.child", path))
        }
        "rate_limit" => {
            require_bucket(&node.bucket, "bucket", path, bucket_names)?;
            if node.reserve_bucket.is_some() {
                require_bucket(&node.reserve_bucket, "reserve_bucket", path, bucket_names)?;
            }
            let c = require_ref(&node.child, "child", path)?;
            validate_ref(c, nodes, bucket_names, visiting, &format!("{}.child", path))
        }
        "htb" => {
            if node.global_bucket.is_some() {
                require_bucket(&node.global_bucket, "global_bucket", path, bucket_names)?;
            }
            require_bucket(&node.high_bucket, "high_bucket", path, bucket_names)?;
            require_bucket(&node.low_bucket, "low_bucket", path, bucket_names)?;
            let hi = require_ref(&node.high, "high", path)?;
            let lo = require_ref(&node.low, "low", path)?;
            validate_ref(hi, nodes, bucket_names, visiting, &format!("{}.high", path))?;
            validate_ref(lo, nodes, bucket_names, visiting, &format!("{}.low", path))
        }
        "prio" => {
            let hi = require_ref(&node.high, "high", path)?;
            let lo = require_ref(&node.low, "low", path)?;
            validate_ref(hi, nodes, bucket_names, visiting, &format!("{}.high", path))?;
            validate_ref(lo, nodes, bucket_names, visiting, &format!("{}.low", path))
        }
        "dual_fair" => {
            let a = require_ref(&node.a, "a", path)?;
            let b = require_ref(&node.b, "b", path)?;
            validate_ref(a, nodes, bucket_names, visiting, &format!("{}.a", path))?;
            validate_ref(b, nodes, bucket_names, visiting, &format!("{}.b", path))
        }
        "sparse" => {
            let sp = require_ref(&node.sparse, "sparse", path)?;
            let bk = require_ref(&node.bulk, "bulk", path)?;
            validate_ref(sp, nodes, bucket_names, visiting, &format!("{}.sparse", path))?;
            validate_ref(bk, nodes, bucket_names, visiting, &format!("{}.bulk", path))
        }
        "drr" | "class_drr" => {
            if let Some(by) = &node.by {
                match by.as_str() {
                    "flow" | "five_tuple" | "src_ip" | "src" | "dst_ip" | "dst" | "class"
                    | "queue_num" | "src_or_dst_by_class" | "auto_ip" => {}
                    other => {
                        return Err(format!(
                            "[qdisc] 节点 {} 的 drr `by = \"{}\"` 不支持（可选：flow / src_ip / dst_ip / class / src_or_dst_by_class）",
                            path, other
                        ));
                    }
                }
            }
            let c = require_ref(&node.child, "child", path)?;
            validate_ref(c, nodes, bucket_names, visiting, &format!("{}.child", path))
        }
        other => Err(format!(
            "[qdisc] 节点 {} 的 type=\"{}\" 不认识（可选：tail_drop / head_drop / ttl_drop / ack_filter / rate_limit / htb / prio / dual_fair / sparse / drr）",
            path, other
        )),
    }
}

/// 构建顶层调度树：
/// - 若配置了 `[qdisc.root]`，按配置递归拼装；
/// - 若未配置 `[qdisc.root]`，打印明确警告并回退到 `TailDropFifo(300)`（外挂 global_bucket 限速）。
pub fn build_root_qdisc(
    nodes: &HashMap<String, QdiscNode>,
    buckets: &HashMap<String, SharedBucket>,
    default_global_bucket: &SharedBucket,
) -> BoxQdisc {
    if !nodes.contains_key("root") {
        println!("⚠️  [调度树] 警告：配置文件未定义 [qdisc.root]，已启用默认兜底树：尾丢弃 TailDropFifo(limit=300)");
        let leaf: BoxQdisc = Box::new(TailDropFifo::new(300));
        return Box::new(RateLimitQdisc::new(
            leaf,
            default_global_bucket.clone(),
            Box::new(|_| 0),
        ));
    }
    println!(
        "🌳 [调度树] 从配置文件 [qdisc.root] 构建自定义调度树（共定义 {} 个命名积木）",
        nodes.len()
    );
    build_ref(
        &QdiscRef::Name("root".to_string()),
        nodes,
        buckets,
        default_global_bucket,
    )
}

fn build_ref(
    r: &QdiscRef,
    nodes: &HashMap<String, QdiscNode>,
    buckets: &HashMap<String, SharedBucket>,
    default_global_bucket: &SharedBucket,
) -> BoxQdisc {
    match r {
        QdiscRef::Name(name) => {
            let node = nodes
                .get(name)
                .unwrap_or_else(|| panic!("[qdisc.{}] 不存在", name));
            build_node(node, nodes, buckets, default_global_bucket)
        }
        QdiscRef::Inline(node) => build_node(node, nodes, buckets, default_global_bucket),
    }
}

fn build_node(
    node: &QdiscNode,
    nodes: &HashMap<String, QdiscNode>,
    buckets: &HashMap<String, SharedBucket>,
    default_global_bucket: &SharedBucket,
) -> BoxQdisc {
    let k = node.kind.trim().to_lowercase();
    let mut q: BoxQdisc = match k.as_str() {
        "tail_drop" | "tail_drop_fifo" | "fifo" => {
            let lim = node.limit.unwrap_or(300);
            Box::new(TailDropFifo::new(lim))
        }
        "head_drop" | "head_drop_fifo" => {
            let lim = node.limit.unwrap_or(300);
            Box::new(HeadDropFifo::new(lim))
        }
        "ttl_drop" => {
            let inner = build_ref(
                node.child.as_ref().expect("ttl_drop.child"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let ms = node.ttl_ms.unwrap_or(100);
            if ms == 0 {
                inner
            } else {
                Box::new(TtlDropWrapper::new(ms, inner))
            }
        }
        "ack_filter" | "tcp_ack_filter" => {
            let inner = build_ref(
                node.child.as_ref().expect("ack_filter.child"),
                nodes,
                buckets,
                default_global_bucket,
            );
            Box::new(TcpAckFilterQdisc::new(inner))
        }
        "rate_limit" => {
            let inner = build_ref(
                node.child.as_ref().expect("rate_limit.child"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let b_name = node.bucket.as_ref().expect("rate_limit.bucket");
            let b = buckets.get(b_name).expect("rate_limit bucket exists").clone();
            let res_b = node
                .reserve_bucket
                .as_ref()
                .and_then(|n| buckets.get(n))
                .cloned();
            Box::new(RateLimitQdisc::new(
                inner,
                b,
                Box::new(move |_| res_b.as_ref().map(|x| x.burst() as usize).unwrap_or(0)),
            ))
        }
        "htb" => {
            let hi = build_ref(
                node.high.as_ref().expect("htb.high"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let lo = build_ref(
                node.low.as_ref().expect("htb.low"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let gl_b = node
                .global_bucket
                .as_ref()
                .and_then(|n| buckets.get(n))
                .unwrap_or(default_global_bucket)
                .clone();
            let hi_b = buckets
                .get(node.high_bucket.as_ref().expect("htb.high_bucket"))
                .expect("htb high_bucket exists")
                .clone();
            let lo_b = buckets
                .get(node.low_bucket.as_ref().expect("htb.low_bucket"))
                .expect("htb low_bucket exists")
                .clone();
            let hi_res = hi_b.burst() as usize;
            let lo_res = lo_b.burst() as usize;
            let hi_classes = node.high_classes.clone().unwrap_or_else(|| vec![2, 3]);
            Box::new(HtbQdisc::new(
                hi,
                lo,
                hi_b,
                lo_b,
                gl_b,
                hi_res,
                lo_res,
                Box::new(move |ctx| hi_classes.contains(&ctx.queue_num)),
            ))
        }
        "prio" => {
            let hi = build_ref(
                node.high.as_ref().expect("prio.high"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let lo = build_ref(
                node.low.as_ref().expect("prio.low"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let hi_classes = node.high_classes.clone().unwrap_or_else(|| vec![2, 3]);
            Box::new(PrioQdisc::new(
                hi,
                lo,
                Box::new(move |ctx| hi_classes.contains(&ctx.queue_num)),
            ))
        }
        "dual_fair" => {
            let a = build_ref(
                node.a.as_ref().expect("dual_fair.a"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let b = build_ref(
                node.b.as_ref().expect("dual_fair.b"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let quantum = node.quantum.unwrap_or(1500);
            let a_classes = node.a_classes.clone().unwrap_or_else(|| vec![2]);
            Box::new(DualFairQdisc::new(
                a,
                b,
                quantum,
                Box::new(move |ctx| a_classes.contains(&ctx.queue_num)),
            ))
        }
        "sparse" => {
            let sp = build_ref(
                node.sparse.as_ref().expect("sparse.sparse"),
                nodes,
                buckets,
                default_global_bucket,
            );
            let bk = build_ref(
                node.bulk.as_ref().expect("sparse.bulk"),
                nodes,
                buckets,
                default_global_bucket,
            );
            Box::new(SparseQdisc::new(sp, bk))
        }
        "drr" | "class_drr" => {
            let by = node.by.clone().unwrap_or_else(|| "flow".to_string());
            let quantum = node.quantum.unwrap_or(1500);
            let child_ref = node.child.as_ref().expect("drr.child").clone();
            let nodes_cl = nodes.clone();
            let buckets_cl = buckets.clone();
            let def_gl_cl = default_global_bucket.clone();
            let factory = Box::new(move || {
                build_ref(&child_ref, &nodes_cl, &buckets_cl, &def_gl_cl)
            });

            match by.as_str() {
                "flow" | "five_tuple" => Box::new(ClassDrrQdisc::new(
                    Box::new(move |ctx: &PacketContext<Message, FiveTuple>| {
                        (ctx.key.clone(), quantum)
                    }),
                    factory,
                )),
                "src_ip" | "src" => Box::new(ClassDrrQdisc::new(
                    Box::new(move |ctx: &PacketContext<Message, FiveTuple>| (ctx.key.src, quantum)),
                    factory,
                )),
                "dst_ip" | "dst" => Box::new(ClassDrrQdisc::new(
                    Box::new(move |ctx: &PacketContext<Message, FiveTuple>| (ctx.key.dst, quantum)),
                    factory,
                )),
                "class" | "queue_num" => Box::new(ClassDrrQdisc::new(
                    Box::new(move |ctx: &PacketContext<Message, FiveTuple>| {
                        (ctx.queue_num, quantum)
                    }),
                    factory,
                )),
                "src_or_dst_by_class" | "auto_ip" => Box::new(ClassDrrQdisc::new(
                    Box::new(move |ctx: &PacketContext<Message, FiveTuple>| match ctx.queue_num {
                        0 | 1 => (ctx.key.dst, quantum),
                        _ => (ctx.key.src, quantum),
                    }),
                    factory,
                )),
                _ => unreachable!(),
            }
        }
        other => panic!("未知的 qdisc 类型 {}", other),
    };

    // 语法糖：如果节点（非独立 ack_filter/ttl_drop）自己带了 ack_filter = true 或 ttl_ms > 0，自动外包
    if k != "ack_filter" && k != "tcp_ack_filter" && node.ack_filter {
        q = Box::new(TcpAckFilterQdisc::new(q));
    }
    if k != "ttl_drop" {
        if let Some(ms) = node.ttl_ms {
            if ms > 0 {
                q = Box::new(TtlDropWrapper::new(ms, q));
            }
        }
    }
    q
}
