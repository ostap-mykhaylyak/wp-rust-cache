use super::*;
use crate::value::*;

fn small() -> Config {
    Config {
        memory: 8 << 20,
        shards: 4,
        max_item_size: 1 << 20,
        // Most tests assert exact eviction behaviour; TinyLFU has its own.
        policy: Policy::Lru,
        ..Config::default()
    }
}

fn setup(cfg: &Config) -> (Cache, &'static [u8], GroupHandle) {
    let c = Cache::anonymous(cfg).unwrap();
    let ns: &'static [u8] = b"site-a";
    let g = c.group(ns, b"options");
    (c, ns, g)
}

fn get(c: &Cache, g: GroupHandle, key: &str) -> Option<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    c.get(&g, 0, key.as_bytes(), &mut out)
        .unwrap()
        .map(|t| (t, out))
}

fn set(c: &Cache, g: GroupHandle, key: &str, v: &[u8]) -> SetOutcome {
    c.set(&g, 0, key.as_bytes(), TAG_STRING, v, 0, SetMode::Set)
        .unwrap()
}

#[test]
fn set_get_delete() {
    let (c, _, g) = setup(&small());
    assert_eq!(get(&c, g, "a"), None);
    assert_eq!(set(&c, g, "a", b"hello"), SetOutcome::Stored);
    assert_eq!(get(&c, g, "a"), Some((TAG_STRING, b"hello".to_vec())));
    // overwrite: smaller, then much larger (reallocation)
    set(&c, g, "a", b"hi");
    assert_eq!(get(&c, g, "a").unwrap().1, b"hi");
    let big = vec![7u8; 5000];
    set(&c, g, "a", &big);
    assert_eq!(get(&c, g, "a").unwrap().1, big);
    assert!(c.delete(&g, 0, b"a").unwrap());
    assert!(!c.delete(&g, 0, b"a").unwrap());
    assert_eq!(get(&c, g, "a"), None);
    // empty value is a value
    set(&c, g, "e", b"");
    assert_eq!(get(&c, g, "e"), Some((TAG_STRING, vec![])));
    assert!(c.verify(false).unwrap().is_empty());
}

#[test]
fn tags_are_preserved() {
    let (c, _, g) = setup(&small());
    let mut out = Vec::new();
    c.set(&g, 0, b"f", TAG_FALSE, b"", 0, SetMode::Set).unwrap();
    assert_eq!(c.get(&g, 0, b"f", &mut out).unwrap(), Some(TAG_FALSE));
    c.set(&g, 0, b"n", TAG_NULL, b"", 0, SetMode::Set).unwrap();
    assert_eq!(c.get(&g, 0, b"n", &mut out).unwrap(), Some(TAG_NULL));
}

#[test]
fn namespaces_groups_and_blogs_are_isolated() {
    let c = Cache::anonymous(&small()).unwrap();
    let a = b"site-a" as &[u8];
    let b = b"site-b" as &[u8];
    let ga = c.group(a, b"posts");
    let gb = c.group(b, b"posts");
    let ga2 = c.group(a, b"terms");
    assert_ne!(ga, gb);
    assert_eq!(c.group(a, b"posts"), ga, "stable ids");
    set(&c, ga, "1", b"a");
    set(&c, gb, "1", b"b");
    assert_eq!(get(&c, ga, "1").unwrap().1, b"a");
    assert_eq!(get(&c, gb, "1").unwrap().1, b"b");
    assert_eq!(get(&c, ga2, "1"), None);
    // blogs
    c.set(&ga, 2, b"1", TAG_STRING, b"blog2", 0, SetMode::Set)
        .unwrap();
    let mut out = Vec::new();
    c.get(&ga, 2, b"1", &mut out).unwrap();
    assert_eq!(out, b"blog2");
    assert_eq!(get(&c, ga, "1").unwrap().1, b"a");
}

#[test]
fn add_and_replace() {
    let (c, _, g) = setup(&small());
    let add = |k: &str| {
        c.set(&g, 0, k.as_bytes(), TAG_STRING, b"x", 0, SetMode::Add)
            .unwrap()
    };
    let rep = |k: &str| {
        c.set(&g, 0, k.as_bytes(), TAG_STRING, b"y", 0, SetMode::Replace)
            .unwrap()
    };
    assert_eq!(rep("k"), SetOutcome::Missing);
    assert_eq!(add("k"), SetOutcome::Stored);
    assert_eq!(add("k"), SetOutcome::Exists);
    assert_eq!(rep("k"), SetOutcome::Stored);
    assert_eq!(get(&c, g, "k").unwrap().1, b"y");
}

#[test]
fn ttl_expires_as_miss() {
    let (c, _, g) = setup(&small());
    c.set(&g, 0, b"t", TAG_STRING, b"v", 1, SetMode::Set)
        .unwrap();
    c.set(&g, 0, b"p", TAG_STRING, b"v", 0, SetMode::Set)
        .unwrap();
    assert!(get(&c, g, "t").is_some());
    std::thread::sleep(std::time::Duration::from_millis(2100));
    assert_eq!(get(&c, g, "t"), None);
    assert!(get(&c, g, "p").is_some());
    // add over an expired key succeeds
    assert_eq!(
        c.set(&g, 0, b"t", TAG_STRING, b"w", 0, SetMode::Add)
            .unwrap(),
        SetOutcome::Stored
    );
    assert!(c.stats().expired >= 1);
}

#[test]
fn incr_decr() {
    let (c, _, g) = setup(&small());
    assert_eq!(c.incr(&g, 0, b"n", 1).unwrap(), None);
    c.set(&g, 0, b"n", TAG_LONG, &10i64.to_ne_bytes(), 0, SetMode::Set)
        .unwrap();
    assert_eq!(c.incr(&g, 0, b"n", 5).unwrap(), Some(Number::Long(15)));
    assert_eq!(c.incr(&g, 0, b"n", -100).unwrap(), Some(Number::Long(0)));
    // string value, entry block too small for 8 bytes → reallocated
    c.set(&g, 0, b"s", TAG_STRING, b"7", 0, SetMode::Set)
        .unwrap();
    assert_eq!(c.incr(&g, 0, b"s", 1).unwrap(), Some(Number::Long(8)));
    let (tag, v) = get(&c, g, "s").unwrap();
    assert_eq!(tag, TAG_LONG);
    assert_eq!(i64::from_ne_bytes(v.try_into().unwrap()), 8);
    assert!(c.verify(false).unwrap().is_empty());
}

#[test]
fn flush_group_namespace_all() {
    let c = Cache::anonymous(&small()).unwrap();
    let a = b"a" as &[u8];
    let b = b"b" as &[u8];
    let posts = c.group(a, b"posts");
    let terms = c.group(a, b"terms");
    let other = c.group(b, b"posts");
    for g in [posts, terms, other] {
        set(&c, g, "k", b"v");
    }
    c.flush_group(&posts).unwrap();
    assert_eq!(get(&c, posts, "k"), None);
    assert!(get(&c, terms, "k").is_some());
    set(&c, posts, "k", b"v2");
    assert_eq!(get(&c, posts, "k").unwrap().1, b"v2");

    c.flush_namespace(a).unwrap();
    assert_eq!(get(&c, posts, "k"), None);
    assert_eq!(get(&c, terms, "k"), None);
    assert!(get(&c, other, "k").is_some(), "other install untouched");

    c.flush_all().unwrap();
    assert_eq!(get(&c, other, "k"), None);
    assert_eq!(c.stats().entries, 0);
}

#[test]
fn stale_entries_are_reclaimed() {
    let cfg = Config {
        memory: 4 << 20,
        shards: 1,
        ..small()
    };
    let (c, ns, g) = setup(&cfg);
    let other = c.group(ns, b"other");
    for i in 0..200 {
        set(&c, g, &format!("k{i}"), &[1u8; 100]);
    }
    c.flush_group(&g).unwrap();
    let usage = c.group_usage().unwrap();
    let u = usage.iter().find(|u| u.name == "options").unwrap();
    assert_eq!(u.entries, 0);
    assert_eq!(u.stale_entries, 200);
    // New writes sweep the LRU tail, which holds the stale entries.
    for i in 0..150 {
        set(&c, other, &format!("n{i}"), b"x");
    }
    assert!(c.stats().stale >= 200, "stale={}", c.stats().stale);
}

#[test]
fn eviction_keeps_structure_valid() {
    let cfg = Config {
        memory: 2 << 20,
        shards: 1,
        max_item_size: 256 << 10,
        ..small()
    };
    let (c, _, g) = setup(&cfg);
    let mut rng = 0x1234_5678u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for i in 0..20_000 {
        let size = match next() % 10 {
            0 => (next() % 60_000) as usize,
            1..=3 => (next() % 4000) as usize,
            _ => (next() % 300) as usize,
        };
        let k = format!("key{}", next() % 3000);
        let v = vec![(i % 251) as u8; size];
        let r = set(&c, g, &k, &v);
        assert!(matches!(r, SetOutcome::Stored), "{r:?}");
        if let Some((_, got)) = get(&c, g, &k) {
            assert_eq!(got, v);
        }
        if i % 7 == 0 {
            c.delete(&g, 0, format!("key{}", next() % 3000).as_bytes())
                .unwrap();
        }
        if i % 2000 == 0 {
            assert_eq!(c.verify(false).unwrap(), vec![]);
        }
    }
    let s = c.stats();
    assert!(s.evictions > 0);
    assert!(s.alloc_bytes <= s.heap_bytes);
    assert_eq!(c.verify(false).unwrap(), vec![]);
}

#[test]
fn too_large_deletes_old_copy() {
    let (c, _, g) = setup(&small());
    set(&c, g, "k", b"old");
    let huge = vec![0u8; 2 << 20];
    assert_eq!(set(&c, g, "k", &huge), SetOutcome::TooLarge);
    assert_eq!(
        get(&c, g, "k"),
        None,
        "a failed write must not leave the old value"
    );
}

#[test]
fn long_keys() {
    let (c, _, g) = setup(&small());
    let k = "x".repeat(10_000);
    set(&c, g, &k, b"v");
    assert_eq!(get(&c, g, &k).unwrap().1, b"v");
}

#[test]
fn tinylfu_admission() {
    let cfg = Config {
        memory: 2 << 20,
        shards: 1,
        policy: Policy::TinyLfu,
        ..small()
    };
    let (c, _, g) = setup(&cfg);
    // hot set, read many times
    for i in 0..200 {
        set(&c, g, &format!("hot{i}"), &[0u8; 1000]);
    }
    for _ in 0..20 {
        for i in 0..200 {
            get(&c, g, &format!("hot{i}"));
        }
    }
    // a scan of one-hit wonders must not flush the hot set
    for i in 0..20_000 {
        set(&c, g, &format!("scan{i}"), &[0u8; 1000]);
    }
    let hot = (0..200)
        .filter(|i| get(&c, g, &format!("hot{i}")).is_some())
        .count();
    assert!(hot > 150, "hot entries surviving: {hot}");
    assert!(c.stats().rejected > 0);
    assert_eq!(c.verify(false).unwrap(), vec![]);
}

#[test]
fn dirty_shard_is_reset() {
    let cfg = Config {
        shards: 1,
        ..small()
    };
    let (c, _, g) = setup(&cfg);
    set(&c, g, "k", b"v");
    c.test_mark_dirty(0);
    assert_eq!(get(&c, g, "k"), None);
    assert_eq!(c.stats().recoveries, 1);
    set(&c, g, "k", b"v");
    assert!(get(&c, g, "k").is_some());
}

// ---- multi-process -----------------------------------------------------------

fn fork(f: impl FnOnce()) -> libc::pid_t {
    // SAFETY: test helper; the child only runs `f` and exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        f();
        // SAFETY: terminate the child without running the test harness.
        unsafe { libc::_exit(0) };
    }
    pid
}

fn wait(pid: libc::pid_t) -> i32 {
    let mut status = 0;
    // SAFETY: waiting for our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    status
}

#[test]
fn concurrent_incr_from_100_processes_is_exact() {
    let (c, _, g) = setup(&small());
    c.set(
        &g,
        0,
        b"counter",
        TAG_LONG,
        &0i64.to_ne_bytes(),
        0,
        SetMode::Set,
    )
    .unwrap();
    let pids: Vec<_> = (0..100)
        .map(|_| {
            fork(|| {
                for _ in 0..1000 {
                    c.incr(&g, 0, b"counter", 1).unwrap().unwrap();
                }
            })
        })
        .collect();
    for p in pids {
        assert_eq!(wait(p), 0);
    }
    let (_, v) = get(&c, g, "counter").unwrap();
    assert_eq!(i64::from_ne_bytes(v.try_into().unwrap()), 100_000);
}

#[test]
fn processes_share_the_segment() {
    let (c, _, g) = setup(&small());
    let pid = fork(|| {
        set(&c, g, "from-child", b"hello");
    });
    wait(pid);
    assert_eq!(get(&c, g, "from-child").unwrap().1, b"hello");
}

#[test]
fn owner_killed_while_holding_lock() {
    let cfg = Config {
        shards: 1,
        ..small()
    };
    let (c, _, g) = setup(&cfg);
    set(&c, g, "k", b"v");
    let pid = fork(|| {
        // Take the shard lock, then hang: the parent kills us with SIGKILL.
        let _ = c.with_shard(0, |_| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            Ok(())
        });
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    // SAFETY: killing our own child.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait(pid);
    // The next locker gets EOWNERDEAD, resets the shard and carries on.
    assert_eq!(get(&c, g, "k"), None);
    assert_eq!(c.stats().recoveries, 1);
    set(&c, g, "k", b"again");
    assert_eq!(get(&c, g, "k").unwrap().1, b"again");
    assert_eq!(c.verify(false).unwrap(), vec![]);
}

#[test]
fn killed_mid_write_storm_leaves_usable_cache() {
    let cfg = Config {
        shards: 2,
        ..small()
    };
    let (c, _, g) = setup(&cfg);
    let mut pids = Vec::new();
    let c = &c;
    for w in 0..8 {
        pids.push(fork(move || {
            let mut i = 0u64;
            loop {
                let k = format!("w{w}-{}", i % 500);
                let _ = c.set(
                    &g,
                    0,
                    k.as_bytes(),
                    TAG_STRING,
                    &vec![1u8; (i % 3000) as usize],
                    0,
                    SetMode::Set,
                );
                let mut out = Vec::new();
                let _ = c.get(&g, 0, k.as_bytes(), &mut out);
                i += 1;
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    for p in &pids {
        // SAFETY: killing our own children.
        unsafe { libc::kill(*p, libc::SIGKILL) };
    }
    for p in pids {
        wait(p);
    }
    // Whatever state the kills left, the next operations must work and the
    // structures must verify (a shard left locked or dirty is reset).
    for i in 0..100 {
        set(c, g, &format!("after{i}"), b"ok");
    }
    for i in 0..100 {
        assert_eq!(get(c, g, &format!("after{i}")).unwrap().1, b"ok");
    }
    assert_eq!(c.verify(false).unwrap(), vec![]);
}

// ---- file-backed segments --------------------------------------------------

fn file_cfg(name: &str) -> Config {
    let dir = std::env::temp_dir();
    Config {
        path_template: dir
            .join(format!("wprc-test-{name}-{}", std::process::id()))
            .display()
            .to_string(),
        preallocate: false,
        // The test container runs as root, which only creates segments for
        // a named owner.
        owner: Some("root".into()),
        ..small()
    }
}

#[test]
fn root_without_owner_does_not_create() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let cfg = Config {
        owner: None,
        ..file_cfg("rootless")
    };
    assert!(matches!(
        Cache::attach(&cfg, AttachMode::Create),
        Err(Error::NotFound(_))
    ));
    assert!(!cfg.path().exists());
}

#[test]
fn file_segment_create_reattach_recreate() {
    let cfg = file_cfg("lifecycle");
    let path = cfg.path();
    let a = Cache::attach(&cfg, AttachMode::Create).unwrap();
    let g = a.group(b"s", b"g");
    set(&a, g, "k", b"v");

    // a second process-like attach sees the data
    let b = Cache::attach(&cfg, AttachMode::Create).unwrap();
    assert_eq!(get(&b, g, "k").unwrap().1, b"v");
    let st = std::fs::metadata(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(st.permissions().mode() & 0o777, 0o600);

    // configuration change: the old segment is retired and replaced
    let cfg2 = Config {
        shards: 2,
        ..cfg.clone()
    };
    let c = Cache::attach(&cfg2, AttachMode::Create).unwrap();
    assert!(a.is_stale() && b.is_stale());
    let ns = b"s" as &[u8];
    let g2 = c.group(ns, b"g");
    assert_eq!(get(&c, g2, "k"), None);

    // the diagnostics attach never recreates
    let d = Cache::attach(&cfg, AttachMode::Existing).unwrap();
    assert_eq!(d.stats().shards, 2);
    drop((a, b, c, d));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn attach_never_waits_long_behind_another_process() {
    let cfg = file_cfg("busy");
    let path = cfg.path();
    drop(Cache::attach(&cfg, AttachMode::Create).unwrap());
    // A child holds the file lock (as a process stuck creating the segment
    // would) while we try to attach.
    let pid = fork(|| {
        let f = std::fs::File::open(&path).unwrap();
        use std::os::unix::io::AsRawFd;
        // SAFETY: plain flock on our own descriptor.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
        std::thread::sleep(std::time::Duration::from_secs(5));
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    let t = std::time::Instant::now();
    let r = Cache::attach(&cfg, AttachMode::Create);
    let waited = t.elapsed();
    // SAFETY: killing our own child.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait(pid);
    assert!(matches!(r, Err(Error::Busy(_))), "{:?}", r.err());
    assert!(
        waited < std::time::Duration::from_millis(1500),
        "waited {waited:?}"
    );
    // Once the lock is gone, attaching works again.
    assert!(Cache::attach(&cfg, AttachMode::Create).is_ok());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn refuses_segment_owned_by_another_user() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return; // needs root to prepare files for another uid
    }
    let cfg = file_cfg("foreign");
    let path = cfg.path();
    drop(Cache::attach(&cfg, AttachMode::Create).unwrap());
    // Planted by uid 1234, readable by group 4321 — which our unprivileged
    // child belongs to. The child can open it, and must refuse it.
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: plain syscalls on a path we created.
    unsafe {
        libc::chown(c.as_ptr(), 1234, 4321);
        libc::chmod(c.as_ptr(), 0o660);
    }
    let cfg_child = Config {
        owner: None,
        ..cfg.clone()
    };
    let pid = fork(move || {
        // SAFETY: dropping privileges in the child before the attempt.
        unsafe {
            libc::setgid(4321);
            libc::setuid(65534);
        }
        let ok = matches!(
            Cache::attach(&cfg_child, AttachMode::Create),
            Err(Error::Permissions(_))
        );
        // SAFETY: report through the exit status.
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    });
    let status = wait(pid);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(status, 0, "a foreign segment was accepted");
}

#[test]
fn refuses_world_accessible_segment() {
    let cfg = file_cfg("perm");
    let path = cfg.path();
    std::fs::write(&path, b"").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        Cache::attach(&cfg, AttachMode::Create),
        Err(Error::Permissions(_))
    ));
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn garbage_file_is_replaced() {
    let cfg = file_cfg("garbage");
    let path = cfg.path();
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(&vec![0xAB; 8192]).unwrap();
    }
    let c = Cache::attach(&cfg, AttachMode::Create).unwrap();
    let ns = b"s" as &[u8];
    let g = c.group(ns, b"g");
    set(&c, g, "k", b"v");
    assert!(get(&c, g, "k").is_some());
    drop(c);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn unlimited_groups_like_woocommerce_products() {
    // WooCommerce uses one group per product ("product_123"). Groups take no
    // shared slot, so their number is unbounded; only the diagnostic name
    // directory fills up, and that changes nothing for caching.
    let cfg = Config {
        group_slots: 64,
        ..small()
    };
    let c = Cache::anonymous(&cfg).unwrap();
    let ns = b"shop" as &[u8];
    for i in 0..50_000 {
        let g = c.group(ns, format!("product_{i}").as_bytes());
        set(&c, g, "wc_product_meta", format!("v{i}").as_bytes());
    }
    for i in (0..50_000).step_by(997) {
        let g = c.group(ns, format!("product_{i}").as_bytes());
        assert_eq!(
            get(&c, g, "wc_product_meta").unwrap().1,
            format!("v{i}").as_bytes()
        );
    }
    assert!(c.stats().groups_overflow > 0);
    // Flushing one product's group leaves (almost all) others alone.
    let g7 = c.group(ns, b"product_7");
    c.flush_group(&g7).unwrap();
    assert_eq!(get(&c, g7, "wc_product_meta"), None);
    let alive = (0..1000)
        .filter(|i| {
            get(
                &c,
                c.group(ns, format!("product_{i}").as_bytes()),
                "wc_product_meta",
            )
            .is_some()
        })
        .count();
    assert!(alive >= 998, "{alive}");
}

#[test]
fn group_keys_lists_one_group_largest_first() {
    let (c, ns, g) = setup(&small());
    let other = c.group(ns, b"other");
    c.set(&g, 0, b"small", TAG_STRING, b"x", 0, SetMode::Set)
        .unwrap();
    c.set(&g, 0, b"big", TAG_STRING, &[7u8; 5000], 60, SetMode::Set)
        .unwrap();
    c.set(
        &g,
        3,
        b"blog3",
        TAG_LONG,
        &1i64.to_ne_bytes(),
        0,
        SetMode::Set,
    )
    .unwrap();
    c.set(&other, 0, b"elsewhere", TAG_STRING, b"y", 0, SetMode::Set)
        .unwrap();
    let keys = c.group_keys(ns, b"options").unwrap();
    let names: Vec<&[u8]> = keys.iter().map(|k| k.key.as_slice()).collect();
    assert_eq!(names.len(), 3);
    assert_eq!(names[0], b"big");
    assert!(keys[0].value_len == 5000 && keys[0].expires > 0 && keys[0].live);
    let b3 = keys.iter().find(|k| k.key == b"blog3").unwrap();
    assert_eq!((b3.blog, b3.tag), (3, TAG_LONG));
    c.flush_group(&g).unwrap();
    assert!(c
        .group_keys(ns, b"options")
        .unwrap()
        .iter()
        .all(|k| !k.live));
}

#[test]
fn find_group_keys_by_name_across_namespaces() {
    let c = Cache::anonymous(&small()).unwrap();
    let long_salt = b"a-salt-much-longer-than-thirty-two-bytes-0123456789" as &[u8];
    let a = c.group(long_salt, b"options");
    let b = c.group(b"site-b", b"options");
    let other = c.group(b"site-b", b"posts");
    c.set(
        &a,
        0,
        b"alloptions",
        TAG_SERIALIZED,
        &[1u8; 3000],
        0,
        SetMode::Set,
    )
    .unwrap();
    c.set(
        &b,
        0,
        b"notoptions",
        TAG_SERIALIZED,
        b"a:0:{}",
        0,
        SetMode::Set,
    )
    .unwrap();
    c.set(&other, 0, b"1", TAG_STRING, b"post", 0, SetMode::Set)
        .unwrap();
    let found = c.find_group_keys(b"options").unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].1.key, b"alloptions");
    assert!(
        found[0].0.ends_with('…'),
        "long namespace shown truncated: {}",
        found[0].0
    );
    assert_eq!(found[1].0, "site-b");
    assert!(c.find_group_keys(b"nothing").unwrap().is_empty());
}
