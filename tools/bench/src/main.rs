//! Engine benchmarks (no PHP in the loop).
//!
//!   wprc-bench engine   [--workload wordpress] [--workers 1,2,4,8,16,32] [--seconds 5] [--memory 256MB]
//!   wprc-bench eviction [--workload woocommerce] [--memory 16MB,32MB,64MB] [--requests 100000]
//!
//! `engine` forks N worker processes on one anonymous shared segment — the
//! same situation as N PHP-FPM workers — and reports throughput, per-op
//! latency percentiles, CPU per request and lock contention.
//! `eviction` replays the same traffic in one process through caches smaller
//! than the working set and compares hit ratios of LRU and TinyLFU.

mod workload;

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::time::{Duration, Instant};
use workload::{Kind, NoObserve, Observe, Rng, Workload};
use wprc_core::config::{parse_size, Policy};
use wprc_core::histogram;
use wprc_core::layout::HIST_BUCKETS;
use wprc_core::{Cache, Config};

struct Hist {
    get: Vec<u64>,
    set: Vec<u64>,
    ops: u64,
    requests: u64,
}

impl Observe for Hist {
    #[inline]
    fn op(&mut self, get: bool, ns: u64, _hit: bool) {
        let h = if get { &mut self.get } else { &mut self.set };
        h[histogram::bucket(ns)] += 1;
        self.ops += 1;
    }
}

impl Hist {
    fn new() -> Hist {
        Hist {
            get: vec![0; HIST_BUCKETS],
            set: vec![0; HIST_BUCKETS],
            ops: 0,
            requests: 0,
        }
    }
    fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity((HIST_BUCKETS * 2 + 2) * 8);
        for v in self
            .get
            .iter()
            .chain(&self.set)
            .chain([&self.ops, &self.requests])
        {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }
    fn add_bytes(&mut self, b: &[u8]) {
        let v: Vec<u64> = b
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        for i in 0..HIST_BUCKETS {
            self.get[i] += v[i];
            self.set[i] += v[HIST_BUCKETS + i];
        }
        self.ops += v[HIST_BUCKETS * 2];
        self.requests += v[HIST_BUCKETS * 2 + 1];
    }
}

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

fn fmt_ns(ns: Option<u64>) -> String {
    match ns {
        None => "-".into(),
        Some(n) if n < 1000 => format!("{n}ns"),
        Some(n) if n < 1_000_000 => format!("{:.1}µs", n as f64 / 1e3),
        Some(n) => format!("{:.1}ms", n as f64 / 1e6),
    }
}

fn cpu_children() -> Duration {
    // SAFETY: `ru` is a valid out-pointer.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru);
        ru
    };
    let t = |tv: libc::timeval| Duration::new(tv.tv_sec as u64, tv.tv_usec as u32 * 1000);
    t(ru.ru_utime) + t(ru.ru_stime)
}

fn engine(args: &[String]) {
    let kind_name = arg(args, "--workload", "wordpress");
    let kind =
        Kind::parse(&kind_name).expect("workload: get|mixed|wordpress|woocommerce|multisite");
    let workers: Vec<usize> = arg(args, "--workers", "1,2,4,8,16,32")
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let seconds: f64 = arg(args, "--seconds", "5").parse().unwrap();
    let memory = parse_size(&arg(args, "--memory", "256MB")).unwrap();
    let shards: u32 = arg(args, "--shards", "64").parse().unwrap();
    let policy = if arg(args, "--policy", "lru") == "tinylfu" {
        Policy::TinyLfu
    } else {
        Policy::Lru
    };
    // SAFETY: sysconf has no preconditions.
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };

    println!(
        "engine benchmark · workload={kind_name} · {} shards · {} · {policy:?} · {cpus} CPUs · {seconds}s per run",
        shards,
        wprc_core::config::format_size(memory)
    );
    println!("latency = time of one Cache::get/set call measured in the worker (includes ~20ns clock overhead)\n");
    println!(
        "{:>7} {:>11} {:>12} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>7} {:>10}",
        "workers",
        "requests/s",
        "ops/s",
        "get p50",
        "get p95",
        "get p99",
        "set p50",
        "set p95",
        "set p99",
        "CPU/req",
        "hit%",
        "contended"
    );
    for &n in &workers {
        let cfg = Config {
            memory,
            shards,
            policy,
            ..Config::default()
        };
        let cache = Cache::anonymous(&cfg).unwrap();
        let mut w = Workload::new(&cache, kind);
        w.preload(&cache);
        let mut rng = Rng::new(42);
        for _ in 0..2_000 {
            w.request(&cache, &mut rng, &mut NoObserve);
        }
        let before = cache.stats();
        let cpu0 = cpu_children();
        let deadline = Duration::from_secs_f64(seconds);
        let mut pipes = Vec::new();
        let mut pids = Vec::new();
        for i in 0..n {
            let mut fds = [0; 2];
            // SAFETY: fds is a valid 2-element array.
            unsafe { libc::pipe(fds.as_mut_ptr()) };
            // SAFETY: the child only uses the shared mapping and its pipe,
            // then leaves with _exit.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: closing the unused read end in the child.
                unsafe { libc::close(fds[0]) };
                let mut h = Hist::new();
                let mut rng = Rng::new(1000 + i as u64);
                let start = Instant::now();
                while start.elapsed() < deadline {
                    w.request(&cache, &mut rng, &mut h);
                    h.requests += 1;
                }
                // SAFETY: fds[1] is our write end.
                let mut f = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                let _ = f.write_all(&h.to_bytes());
                drop(f);
                // SAFETY: leave without running the parent's destructors.
                unsafe { libc::_exit(0) };
            }
            // SAFETY: closing the write end in the parent.
            unsafe { libc::close(fds[1]) };
            pipes.push(fds[0]);
            pids.push(pid);
        }
        let mut total = Hist::new();
        for fd in pipes {
            // SAFETY: fd is our read end.
            let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            if !b.is_empty() {
                total.add_bytes(&b);
            }
        }
        for pid in pids {
            let mut st = 0;
            // SAFETY: waiting for our children.
            unsafe { libc::waitpid(pid, &mut st, 0) };
        }
        let cpu = cpu_children() - cpu0;
        let after = cache.stats();
        let hits = after.hits - before.hits;
        let misses = after.misses - before.misses;
        let contended = after.contended - before.contended;
        let q = |h: &[u64], p: f64| histogram::quantile(h, p);
        println!(
            "{:>7} {:>11.0} {:>12.0} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>6.1}% {:>9.2}‰",
            n,
            total.requests as f64 / seconds,
            total.ops as f64 / seconds,
            fmt_ns(q(&total.get, 0.5)),
            fmt_ns(q(&total.get, 0.95)),
            fmt_ns(q(&total.get, 0.99)),
            fmt_ns(q(&total.set, 0.5)),
            fmt_ns(q(&total.set, 0.95)),
            fmt_ns(q(&total.set, 0.99)),
            fmt_ns(Some(cpu.as_nanos() as u64 / total.requests.max(1))),
            100.0 * hits as f64 / (hits + misses).max(1) as f64,
            1000.0 * contended as f64 / total.ops.max(1) as f64,
        );
    }
}

fn eviction(args: &[String]) {
    let kind_name = arg(args, "--workload", "woocommerce");
    let kind = Kind::parse(&kind_name).expect("workload");
    let requests: u64 = arg(args, "--requests", "100000").parse().unwrap();
    let sizes: Vec<u64> = arg(args, "--memory", "16MB,32MB,64MB,128MB")
        .split(',')
        .map(|s| parse_size(s).unwrap())
        .collect();
    let shards: u32 = arg(args, "--shards", "16").parse().unwrap();
    println!("eviction benchmark · workload={kind_name} · {requests} requests (first 20% warm-up) · {shards} shards\n");
    println!(
        "{:>8} {:>8} {:>9} {:>11} {:>10} {:>10} {:>10}",
        "memory", "policy", "hit%", "misses/req", "evictions", "rejected", "req/s"
    );
    for &mem in &sizes {
        for policy in [Policy::Lru, Policy::TinyLfu] {
            let cfg = Config {
                memory: mem,
                shards,
                policy,
                max_item_size: 1 << 20,
                ..Config::default()
            };
            let cache = Cache::anonymous(&cfg).unwrap();
            let mut w = Workload::new(&cache, kind);
            w.preload(&cache);
            let mut rng = Rng::new(7);
            let warm = requests / 5;
            for _ in 0..warm {
                w.request(&cache, &mut rng, &mut NoObserve);
            }
            let before = cache.stats();
            let t = Instant::now();
            for _ in warm..requests {
                w.request(&cache, &mut rng, &mut NoObserve);
            }
            let secs = t.elapsed().as_secs_f64();
            let s = cache.stats();
            let (hits, misses) = (s.hits - before.hits, s.misses - before.misses);
            let measured = requests - warm;
            println!(
                "{:>8} {:>8} {:>8.2}% {:>11.2} {:>10} {:>10} {:>10.0}",
                wprc_core::config::format_size(mem),
                policy.name(),
                100.0 * hits as f64 / (hits + misses).max(1) as f64,
                misses as f64 / measured as f64,
                s.evictions - before.evictions,
                s.rejected - before.rejected,
                measured as f64 / secs,
            );
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("engine") => engine(&args),
        Some("eviction") => eviction(&args),
        _ => {
            eprintln!("usage: wprc-bench engine|eviction [options] (see source header)");
            std::process::exit(2);
        }
    }
}
