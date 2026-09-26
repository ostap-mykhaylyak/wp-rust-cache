//! WordPress-shaped traffic.
//!
//! A "request" performs the cache operations a typical page view performs,
//! with sizes taken from real installs: `alloptions` read on every request,
//! post / post-meta / term / user lookups following a Zipf popularity curve,
//! transients with a TTL, and — for WooCommerce — a per-visitor session that
//! is read and rewritten on every request (the churn that stresses eviction).
//! A miss is followed by a set, as WordPress does after loading from MySQL.

use wprc_core::{value::TAG_STRING, Cache, GroupHandle, SetMode};

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }
    #[inline]
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    #[inline]
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Zipf(s) over 0..n via an inverse CDF table.
pub struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    pub fn new(n: usize, s: f64) -> Zipf {
        let mut cdf = Vec::with_capacity(n);
        let mut sum = 0.0;
        for i in 1..=n {
            sum += 1.0 / (i as f64).powf(s);
            cdf.push(sum);
        }
        for c in &mut cdf {
            *c /= sum;
        }
        Zipf { cdf }
    }
    #[inline]
    pub fn sample(&self, rng: &mut Rng) -> u64 {
        let u = rng.unit();
        self.cdf.partition_point(|&c| c < u) as u64
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 100 % hits on a preloaded key set.
    Get,
    /// 90 % get / 10 % set on a preloaded key set.
    Mixed,
    WordPress,
    WooCommerce,
    Multisite,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "get" => Kind::Get,
            "mixed" => Kind::Mixed,
            "wordpress" => Kind::WordPress,
            "woocommerce" => Kind::WooCommerce,
            "multisite" => Kind::Multisite,
            _ => return None,
        })
    }
}

pub struct Groups {
    options: GroupHandle,
    posts: GroupHandle,
    post_meta: GroupHandle,
    terms: GroupHandle,
    term_meta: GroupHandle,
    users: GroupHandle,
    user_meta: GroupHandle,
    transient: GroupHandle,
    wc_session: GroupHandle,
    wc_products: GroupHandle,
    micro: GroupHandle,
}

pub struct Workload {
    pub kind: Kind,
    g: Groups,
    posts: Zipf,
    terms: Zipf,
    users: Zipf,
    transients: Zipf,
    visitors: Zipf,
    products: Zipf,
    micro: Zipf,
    value: Vec<u8>,
    key: Vec<u8>,
    out: Vec<u8>,
}

/// Per-op observer: (is_get, nanoseconds, hit).
pub trait Observe {
    fn op(&mut self, get: bool, ns: u64, hit: bool);
}

pub struct NoObserve;
impl Observe for NoObserve {
    #[inline]
    fn op(&mut self, _: bool, _: u64, _: bool) {}
}

pub const MICRO_KEYS: u64 = 100_000;

impl Workload {
    pub fn new(cache: &Cache, kind: Kind) -> Workload {
        let g = |n: &str| cache.group(b"bench", n.as_bytes());
        Workload {
            kind,
            g: Groups {
                options: g("options"),
                posts: g("posts"),
                post_meta: g("post_meta"),
                terms: g("terms"),
                term_meta: g("term_meta"),
                users: g("users"),
                user_meta: g("user_meta"),
                transient: g("transient"),
                wc_session: g("wc_session_id"),
                wc_products: g("products"),
                micro: g("micro"),
            },
            posts: Zipf::new(20_000, 1.0),
            terms: Zipf::new(800, 0.9),
            users: Zipf::new(2_000, 1.0),
            transients: Zipf::new(300, 1.0),
            visitors: Zipf::new(200_000, 0.6),
            products: Zipf::new(5_000, 1.0),
            micro: Zipf::new(MICRO_KEYS as usize, 0.99),
            value: vec![b'x'; 200_000],
            key: Vec::with_capacity(64),
            out: Vec::with_capacity(256 << 10),
        }
    }

    pub fn preload(&mut self, cache: &Cache) {
        if matches!(self.kind, Kind::Get | Kind::Mixed) {
            for i in 0..MICRO_KEYS {
                self.set_key("k", i);
                cache
                    .set(
                        &self.g.micro,
                        0,
                        &self.key,
                        TAG_STRING,
                        &self.value[..100],
                        0,
                        SetMode::Set,
                    )
                    .unwrap();
            }
        }
    }

    fn set_key(&mut self, prefix: &str, id: u64) {
        use std::io::Write;
        self.key.clear();
        let _ = write!(self.key, "{prefix}{id}");
    }

    #[inline]
    fn get<O: Observe>(&mut self, c: &Cache, gid: GroupHandle, blog: u32, obs: &mut O) -> bool {
        let t = std::time::Instant::now();
        let hit = c
            .get(&gid, blog, &self.key, &mut self.out)
            .ok()
            .flatten()
            .is_some();
        obs.op(true, t.elapsed().as_nanos() as u64, hit);
        hit
    }

    #[inline]
    fn set<O: Observe>(
        &mut self,
        c: &Cache,
        gid: GroupHandle,
        blog: u32,
        size: usize,
        ttl: u32,
        obs: &mut O,
    ) {
        let t = std::time::Instant::now();
        let _ = c.set(
            &gid,
            blog,
            &self.key,
            TAG_STRING,
            &self.value[..size],
            ttl,
            SetMode::Set,
        );
        obs.op(false, t.elapsed().as_nanos() as u64, false);
    }

    /// get, and on a miss "load from the database" and set.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn fetch<O: Observe>(
        &mut self,
        c: &Cache,
        gid: GroupHandle,
        blog: u32,
        prefix: &str,
        id: u64,
        size: usize,
        ttl: u32,
        obs: &mut O,
    ) {
        self.set_key(prefix, id);
        if !self.get(c, gid, blog, obs) {
            self.set(c, gid, blog, size, ttl, obs);
        }
    }

    /// One unit of work: a page view, or 20 operations for the micro loads.
    pub fn request<O: Observe>(&mut self, c: &Cache, rng: &mut Rng, obs: &mut O) {
        let g = self.g.micro;
        match self.kind {
            Kind::Get | Kind::Mixed => {
                for _ in 0..20 {
                    let id = self.micro.sample(rng);
                    self.set_key("k", id);
                    if self.kind == Kind::Mixed && rng.below(10) == 0 {
                        self.set(c, g, 0, 100, 0, obs);
                    } else {
                        self.get(c, g, 0, obs);
                    }
                }
                return;
            }
            _ => {}
        }
        let blog = if self.kind == Kind::Multisite {
            1 + rng.below(3) as u32
        } else {
            0
        };
        let gr = &self.g;
        let (options, posts, post_meta, terms, term_meta, users, user_meta, transient) = (
            gr.options,
            gr.posts,
            gr.post_meta,
            gr.terms,
            gr.term_meta,
            gr.users,
            gr.user_meta,
            gr.transient,
        );
        // Options: alloptions on every request, a few non-autoloaded ones.
        self.fetch(c, options, blog, "alloptions", 0, 120_000, 0, obs);
        self.fetch(c, options, blog, "notoptions", 0, 200, 0, obs);
        for _ in 0..8 {
            let id = rng.below(60);
            self.fetch(c, options, blog, "opt", id, 300, 0, obs);
        }
        self.fetch(c, posts, blog, "last_changed", 0, 30, 0, obs);
        for _ in 0..10 {
            let id = self.posts.sample(rng);
            self.fetch(c, posts, blog, "", id, 2_500, 0, obs);
            self.fetch(c, post_meta, blog, "", id, 3_000, 0, obs);
        }
        for _ in 0..5 {
            let id = self.terms.sample(rng);
            self.fetch(c, terms, blog, "", id, 400, 0, obs);
            self.fetch(c, term_meta, blog, "", id, 300, 0, obs);
        }
        for _ in 0..2 {
            let id = self.users.sample(rng);
            // users and user_meta are global groups in multisite
            self.fetch(c, users, 0, "", id, 1_200, 0, obs);
            self.fetch(c, user_meta, 0, "", id, 2_500, 0, obs);
        }
        for _ in 0..2 {
            let id = self.transients.sample(rng);
            let size = 500 + (id as usize * 997) % 50_000;
            self.fetch(c, transient, blog, "t", id, size, 3_600, obs);
        }
        // An editor saves a post now and then.
        if rng.below(50) == 0 {
            let id = self.posts.sample(rng);
            self.set_key("", id);
            self.set(c, posts, blog, 2_500, 0, obs);
            self.set_key("last_changed", 0);
            self.set(c, posts, blog, 30, 0, obs);
        }
        if self.kind == Kind::WooCommerce {
            let (session, products) = (self.g.wc_session, self.g.wc_products);
            let v = self.visitors.sample(rng);
            self.set_key("session_", v);
            self.get(c, session, blog, obs);
            self.set(c, session, blog, 1_500, 172_800, obs);
            for _ in 0..6 {
                let id = self.products.sample(rng);
                self.fetch(c, products, blog, "p", id, 4_000, 0, obs);
                self.fetch(c, post_meta, blog, "pm", id, 4_000, 0, obs);
            }
        }
    }
}
