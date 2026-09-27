//! `wp-rust-cache`: status, statistics, maintenance and installation.

mod install;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use wprc_core::config::{format_size, DEFAULT_CONFIG_PATH};
use wprc_core::{AttachMode, Cache, Config, Error, Stats};

const USAGE: &str = "\
wp-rust-cache — shared-memory object cache for WordPress

Usage:
  wp-rust-cache status                 is the segment there, how full, how fast
  wp-rust-cache stats [--groups] [--all] [--json | --prometheus]
                                       every counter; --groups adds per-group usage
                                       (numbered groups such as product_123 folded into
                                       product_*, top 30 unless --all);
                                       --prometheus prints node_exporter textfile format
  wp-rust-cache stats --keys GROUP [--namespace NS] [--all]
                                       the largest keys of a group, read from the segment
  wp-rust-cache flush [--namespace NS] empty the segment (or one WordPress install)
  wp-rust-cache verify [--repair]      check every shard's structures
  wp-rust-cache recreate               retire the segment; workers start a new one
  wp-rust-cache install --wp PATH [--user USER] [--extension FILE] [--dry-run] [--force]
                                       enable the extension and the drop-in, then self-test
  wp-rust-cache uninstall --wp PATH [--dry-run]
                                       remove the drop-in, disable the extension, drop the segment
  wp-rust-cache version

Options:
  --config FILE    configuration (default /etc/wp-rust-cache/config.toml)
  --segment PATH   segment path, overriding [shared_memory] path
";

pub struct Args {
    pub cmd: String,
    pub flags: Vec<String>,
    pub values: Vec<(String, String)>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut it = std::env::args().skip(1);
        let mut cmd = String::new();
        let mut flags = Vec::new();
        let mut values = Vec::new();
        const WITH_VALUE: [&str; 7] = [
            "--config",
            "--segment",
            "--namespace",
            "--wp",
            "--user",
            "--extension",
            "--keys",
        ];
        while let Some(a) = it.next() {
            if let Some((k, v)) = a.split_once('=').filter(|(k, _)| k.starts_with("--")) {
                values.push((k.to_string(), v.to_string()));
            } else if WITH_VALUE.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                values.push((a, v));
            } else if a.starts_with("--") {
                flags.push(a);
            } else if cmd.is_empty() {
                cmd = a;
            } else {
                return Err(format!("unexpected argument {a:?}"));
            }
        }
        Ok(Args { cmd, flags, values })
    }

    pub fn flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    pub fn value(&self, k: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(x, _)| x == k)
            .map(|(_, v)| v.as_str())
    }

    pub fn config(&self) -> Result<Config, String> {
        let path = self.value("--config").unwrap_or(DEFAULT_CONFIG_PATH);
        let mut cfg = Config::load(std::path::Path::new(path))?;
        if let Some(s) = self.value("--segment") {
            cfg.path_template = s.to_string();
        }
        Ok(cfg)
    }
}

fn main() -> ExitCode {
    // Behave like other Unix tools when piped into `head`: exit quietly.
    // SAFETY: restoring the default disposition of a signal at startup.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let r = match args.cmd.as_str() {
        "status" => status(&args),
        "stats" => stats(&args),
        "flush" => flush(&args),
        "verify" => verify(&args),
        "recreate" => recreate(&args),
        "install" => install::run(&args),
        "uninstall" => install::uninstall(&args),
        "version" | "--version" | "-V" => {
            println!("wp-rust-cache {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "" | "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn open(args: &Args) -> Result<(Config, Cache), String> {
    let cfg = args.config()?;
    let cache = Cache::attach(&cfg, AttachMode::Existing).map_err(|e| e.to_string())?;
    Ok((cfg, cache))
}

pub fn latency(ns: Option<u64>) -> String {
    match ns {
        None => "n/a".into(),
        Some(n) if n < 1000 => format!("{n} ns"),
        Some(n) => format!("{:.1} µs", n as f64 / 1000.0),
    }
}

fn row(k: &str, v: impl std::fmt::Display) {
    println!("{:<14}{v}", format!("{k}:"));
}

pub fn print_status(s: &Stats) {
    println!("WP Rust Cache\n");
    row("Status", "RUNNING");
    row("Backend", format!("shared-memory ({})", s.path));
    row(
        "Memory",
        format!(
            "{} / {}",
            format_size(s.alloc_bytes),
            format_size(s.heap_bytes)
        ),
    );
    row("Entries", s.entries);
    row(
        "Hit ratio",
        s.hit_ratio()
            .map(|r| format!("{:.2}%", r * 100.0))
            .unwrap_or_else(|| "n/a".into()),
    );
    row("Evictions", s.evictions);
    row("P50", latency(s.latency(true, 0.50)));
    row("P95", latency(s.latency(true, 0.95)));
    row("P99", latency(s.latency(true, 0.99)));
}

fn status(args: &Args) -> Result<(), String> {
    let cfg = args.config()?;
    match Cache::attach(&cfg, AttachMode::Existing) {
        Ok(c) => {
            print_status(&c.stats());
            Ok(())
        }
        Err(Error::NotFound(why)) => {
            println!("WP Rust Cache\n");
            row("Status", "NOT CREATED");
            row("Segment", cfg.path().display());
            row("Reason", why);
            println!("\nThe first PHP request that uses the cache creates it.");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn stats(args: &Args) -> Result<(), String> {
    let (_, c) = open(args)?;
    if let Some(group) = args.value("--keys") {
        return keys(args, &c, group);
    }
    let s = c.stats();
    let counters: Vec<(&str, u64)> = vec![
        ("hits", s.hits),
        ("misses", s.misses),
        ("sets", s.sets),
        ("deletes", s.deletes),
        ("evictions", s.evictions),
        ("expired", s.expired),
        ("stale_reclaimed", s.stale),
        ("admission_rejected", s.rejected),
        ("too_large", s.too_large),
        ("no_memory", s.no_memory),
        ("entries", s.entries),
        ("payload_bytes", s.payload_bytes),
        ("allocated_bytes", s.alloc_bytes),
        ("heap_bytes", s.heap_bytes),
        ("segment_bytes", s.total_size),
        ("max_item_size", s.max_item_size),
        ("shards", s.shards as u64),
        ("namespaces", s.namespaces as u64),
        ("groups", s.groups_used as u64),
        ("group_slots", s.group_slots as u64),
        ("groups_overflow", s.groups_overflow),
        ("lock_contended", s.contended),
        ("shard_resets", s.resets),
        ("recoveries", s.recoveries),
        ("attaches", s.attaches),
        ("created_at", s.created_at),
        ("latency_samples_get", s.samples(true)),
        ("latency_samples_set", s.samples(false)),
    ];
    let quantiles = [("p50", 0.50), ("p95", 0.95), ("p99", 0.99)];
    let groups = if args.flag("--groups") {
        Some(c.group_usage().map_err(|e| e.to_string())?)
    } else {
        None
    };

    if args.flag("--prometheus") {
        print!("{}", prometheus(&s, groups.as_deref()));
        return Ok(());
    }

    if args.flag("--json") {
        let mut parts: Vec<String> = vec![
            format!("\"path\":{}", json_escape(&s.path)),
            format!("\"policy\":{}", json_escape(s.policy)),
        ];
        parts.extend(counters.iter().map(|(k, v)| format!("\"{k}\":{v}")));
        parts.push(format!(
            "\"hit_ratio\":{}",
            s.hit_ratio()
                .map(|r| format!("{r:.6}"))
                .unwrap_or_else(|| "null".into())
        ));
        for (op, get) in [("get", true), ("set", false)] {
            for (q, v) in quantiles {
                let val = s
                    .latency(get, v)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "null".into());
                parts.push(format!("\"{op}_{q}_ns\":{val}"));
            }
        }
        if let Some(gs) = &groups {
            let items: Vec<String> = gs
                .iter()
                .map(|g| {
                    format!(
                        "{{\"namespace\":{},\"group\":{},\"entries\":{},\"bytes\":{},\"stale_entries\":{},\"stale_bytes\":{}}}",
                        json_escape(&g.namespace),
                        json_escape(&g.name),
                        g.entries,
                        g.bytes,
                        g.stale_entries,
                        g.stale_bytes
                    )
                })
                .collect();
            parts.push(format!("\"groups_usage\":[{}]", items.join(",")));
        }
        println!("{{{}}}", parts.join(","));
        return Ok(());
    }

    print_status(&s);
    println!();
    row("Policy", s.policy);
    for (k, v) in &counters {
        println!("  {k:<22}{v}");
    }
    println!();
    println!(
        "  {:<22}{:>10} {:>10} {:>10}",
        "latency (sampled 1/64)", "P50", "P95", "P99"
    );
    for (op, get) in [("get", true), ("set", false)] {
        println!(
            "  {:<22}{:>10} {:>10} {:>10}",
            op,
            latency(s.latency(get, 0.50)),
            latency(s.latency(get, 0.95)),
            latency(s.latency(get, 0.99))
        );
    }
    if let Some(gs) = groups {
        let rows = view::aggregate(&gs);
        let shown = if args.flag("--all") {
            rows.len()
        } else {
            rows.len().min(TOP)
        };
        println!();
        println!(
            "  {:<18} {:<32} {:>7} {:>9} {:>11} {:>7}",
            "namespace", "group", "groups", "entries", "memory", "stale"
        );
        for r in &rows[..shown] {
            println!(
                "  {:<18} {:<32} {:>7} {:>9} {:>11} {:>7}",
                cut(&r.namespace, 18),
                cut(&r.name, 32),
                r.groups,
                r.entries,
                format_size(r.bytes),
                r.stale
            );
        }
        if shown < rows.len() {
            println!("  … {} more rows (--all lists them)", rows.len() - shown);
        }
    }
    Ok(())
}

/// Rows printed by `--groups` and `--keys` unless `--all` is given.
const TOP: usize = 30;

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(n - 1).collect();
        o.push('…');
        o
    }
}

/// `stats --keys GROUP`: the largest keys of a group, read from the segment.
fn keys(args: &Args, c: &Cache, group: &str) -> Result<(), String> {
    let found: Vec<(String, wprc_core::KeyUsage)> = match args.value("--namespace") {
        Some(ns) => c
            .group_keys(ns.as_bytes(), group.as_bytes())
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|k| (ns.to_string(), k))
            .collect(),
        None => c
            .find_group_keys(group.as_bytes())
            .map_err(|e| e.to_string())?,
    };
    if found.is_empty() {
        println!("No keys in group {group:?}.");
        return Ok(());
    }
    let now = wprc_core::now_secs();
    let total: u64 = found.iter().map(|(_, k)| k.alloc as u64).sum();
    let shown = if args.flag("--all") {
        found.len()
    } else {
        found.len().min(TOP)
    };
    println!(
        "Group {group}: {} keys, {}\n",
        found.len(),
        format_size(total)
    );
    println!(
        "  {:<18} {:<44} {:>5} {:<10} {:>10} {:>10} {:>8}",
        "namespace", "key", "blog", "type", "value", "memory", "ttl"
    );
    for (ns, k) in &found[..shown] {
        let ttl = match k.expires {
            0 => "-".to_string(),
            e if e <= now => "expired".to_string(),
            e => format!("{}s", e - now),
        };
        println!(
            "  {:<18} {:<44} {:>5} {:<10} {:>10} {:>10} {:>8}{}",
            cut(ns, 18),
            cut(&String::from_utf8_lossy(&k.key), 44),
            k.blog,
            wprc_core::value::tag_name(k.tag),
            format_size(k.value_len as u64),
            format_size(k.alloc as u64),
            ttl,
            if k.live {
                ""
            } else {
                "  (flushed, awaiting reclamation)"
            }
        );
    }
    if shown < found.len() {
        println!("  … {} more keys (--all lists them)", found.len() - shown);
    }
    Ok(())
}

fn flush(args: &Args) -> Result<(), String> {
    let (_, c) = open(args)?;
    match args.value("--namespace") {
        Some(ns) => {
            let known = c.namespaces();
            if !known.iter().any(|n| n == ns) {
                eprintln!(
                    "note: {ns:?} is not among the recorded namespaces ({}); flushing anyway",
                    known.join(", ")
                );
            }
            c.flush_namespace(ns.as_bytes())
                .map_err(|e| e.to_string())?;
            println!("Namespace {ns} flushed.");
        }
        None => {
            c.flush_all().map_err(|e| e.to_string())?;
            println!("Segment flushed.");
        }
    }
    Ok(())
}

fn verify(args: &Args) -> Result<(), String> {
    let (_, c) = open(args)?;
    let repair = args.flag("--repair");
    let problems = c.verify(repair).map_err(|e| e.to_string())?;
    if problems.is_empty() {
        println!("All {} shards are consistent.", c.shard_count());
        return Ok(());
    }
    for (shard, p) in &problems {
        println!("shard {shard}: {p}{}", if repair { " (reset)" } else { "" });
    }
    if repair {
        Ok(())
    } else {
        Err(format!(
            "{} shard(s) inconsistent; run with --repair to reset them",
            problems.len()
        ))
    }
}

fn recreate(args: &Args) -> Result<(), String> {
    let (cfg, c) = open(args)?;
    c.retire().map_err(|e| e.to_string())?;
    println!(
        "Segment {} retired. PHP workers switch to a new, empty segment at their next request.",
        cfg.path().display()
    );
    Ok(())
}

pub fn config_path(args: &Args) -> PathBuf {
    PathBuf::from(args.value("--config").unwrap_or(DEFAULT_CONFIG_PATH))
}

/// Prometheus text exposition format, for node_exporter's textfile collector
/// (`wp-rust-cache stats --prometheus > /var/lib/node_exporter/wp_rust_cache.prom`).
fn prometheus(s: &Stats, groups: Option<&[wprc_core::GroupUsage]>) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, value: f64| {
        let _ = writeln!(o, "# HELP wp_rust_cache_{name} {help}");
        let _ = writeln!(o, "# TYPE wp_rust_cache_{name} {kind}");
        let _ = writeln!(o, "wp_rust_cache_{name} {value}");
    };
    metric(
        "hits_total",
        "counter",
        "Lookups that found a value.",
        s.hits as f64,
    );
    metric(
        "misses_total",
        "counter",
        "Lookups that found nothing.",
        s.misses as f64,
    );
    metric("sets_total", "counter", "Values stored.", s.sets as f64);
    metric(
        "deletes_total",
        "counter",
        "Values deleted.",
        s.deletes as f64,
    );
    metric(
        "evictions_total",
        "counter",
        "Entries evicted to make room.",
        s.evictions as f64,
    );
    metric(
        "expired_total",
        "counter",
        "Entries dropped at expiry.",
        s.expired as f64,
    );
    metric(
        "stale_reclaimed_total",
        "counter",
        "Flushed entries reclaimed.",
        s.stale as f64,
    );
    metric(
        "admission_rejected_total",
        "counter",
        "New keys refused by TinyLFU.",
        s.rejected as f64,
    );
    metric(
        "too_large_total",
        "counter",
        "Values refused for exceeding max_item_size.",
        s.too_large as f64,
    );
    metric(
        "lock_contended_total",
        "counter",
        "Shard lock acquisitions that had to wait.",
        s.contended as f64,
    );
    metric(
        "recoveries_total",
        "counter",
        "Shards reset after a crash or corruption.",
        s.recoveries as f64,
    );
    metric("entries", "gauge", "Entries stored.", s.entries as f64);
    metric(
        "allocated_bytes",
        "gauge",
        "Bytes of allocated blocks.",
        s.alloc_bytes as f64,
    );
    metric(
        "capacity_bytes",
        "gauge",
        "Bytes available to entries.",
        s.heap_bytes as f64,
    );
    metric(
        "attaches_total",
        "counter",
        "Processes that attached to the segment.",
        s.attaches as f64,
    );
    for (op, get) in [("get", true), ("set", false)] {
        for (q, v) in [("0.5", 0.50), ("0.95", 0.95), ("0.99", 0.99)] {
            if let Some(ns) = s.latency(get, v) {
                let _ = writeln!(
                    o,
                    "wp_rust_cache_latency_seconds{{op=\"{op}\",quantile=\"{q}\"}} {}",
                    ns as f64 / 1e9
                );
            }
        }
    }
    if let Some(gs) = groups {
        let _ = writeln!(o, "# HELP wp_rust_cache_group_bytes Bytes held per group.");
        let _ = writeln!(o, "# TYPE wp_rust_cache_group_bytes gauge");
        // Numbered groups (one per WooCommerce product) are folded into
        // families: one label per product would explode the series count.
        for r in view::aggregate(gs) {
            let esc = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = writeln!(
                o,
                "wp_rust_cache_group_bytes{{namespace=\"{}\",group=\"{}\"}} {}",
                esc(&r.namespace),
                esc(&r.name),
                r.bytes
            );
        }
    }
    o
}
