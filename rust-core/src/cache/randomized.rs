//! Randomized tests: the engine against a reference model, and against
//! deliberate memory corruption.

use super::*;
use crate::value::*;
use std::collections::HashMap;

fn small() -> Config {
    Config {
        memory: 8 << 20,
        shards: 4,
        max_item_size: 1 << 20,
        policy: Policy::Lru,
        ..Config::default()
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

type Model = HashMap<(usize, u32, String), (u8, Vec<u8>)>;

/// Random operations against the cache and a HashMap model.
/// `exact`: the cache never evicts, so every result must match the model.
/// Otherwise a lookup may miss, but a hit must return the latest value.
fn run_model(c: &Cache, seed: u64, steps: usize, exact: bool, max_value: u64) {
    let mut rng = Rng(seed);
    let groups: Vec<GroupHandle> = (0..4)
        .map(|i| c.group(b"model", format!("g{i}").as_bytes()))
        .collect();
    let mut model = Model::new();
    let mut out = Vec::new();
    for step in 0..steps {
        let gi = rng.below(4) as usize;
        let g = &groups[gi];
        let blog = rng.below(2) as u32;
        let key = format!("k{}", rng.below(300));
        let mk = (gi, blog, key.clone());
        match rng.below(100) {
            0..=39 => {
                let got = c.get(g, blog, key.as_bytes(), &mut out).unwrap();
                match (got, model.get(&mk)) {
                    (Some(tag), Some((mt, mv))) => {
                        assert_eq!((tag, &out), (*mt, mv), "step {step}: stale or wrong value")
                    }
                    (Some(_), None) => panic!("step {step}: hit for a key the model lacks"),
                    (None, Some(_)) if exact => panic!("step {step}: miss for a stored key"),
                    (None, Some(_)) => {
                        model.remove(&mk);
                    }
                    (None, None) => {}
                }
            }
            40..=77 => {
                let mode = match rng.below(100) {
                    0..=69 => SetMode::Set,
                    70..=84 => SetMode::Add,
                    _ => SetMode::Replace,
                };
                let (tag, bytes) = if rng.below(4) == 0 {
                    (
                        TAG_LONG,
                        (rng.below(1000) as i64 - 500).to_ne_bytes().to_vec(),
                    )
                } else {
                    let n = rng.below(max_value) as usize;
                    (TAG_STRING, vec![(step % 251) as u8; n])
                };
                let r = c
                    .set(g, blog, key.as_bytes(), tag, &bytes, 0, mode)
                    .unwrap();
                let had = model.contains_key(&mk);
                match r {
                    SetOutcome::Stored => {
                        if exact {
                            match mode {
                                SetMode::Add => {
                                    assert!(!had, "step {step}: add over an existing key")
                                }
                                SetMode::Replace => {
                                    assert!(had, "step {step}: replace of a missing key")
                                }
                                SetMode::Set => {}
                            }
                        }
                        model.insert(mk, (tag, bytes));
                    }
                    SetOutcome::Exists => {
                        assert!(
                            !exact || had,
                            "step {step}: add refused but the model is empty"
                        )
                    }
                    SetOutcome::Missing => {
                        assert!(
                            !exact || !had,
                            "step {step}: replace refused but the model has it"
                        );
                        model.remove(&mk);
                    }
                    other => {
                        assert!(!exact, "step {step}: {other:?} without memory pressure");
                        model.remove(&mk);
                    }
                }
            }
            78..=87 => {
                let deleted = c.delete(g, blog, key.as_bytes()).unwrap();
                let had = model.remove(&mk).is_some();
                if exact {
                    assert_eq!(deleted, had, "step {step}: delete");
                } else {
                    assert!(
                        !deleted || had,
                        "step {step}: deleted a key the model lacks"
                    );
                }
            }
            88..=96 => {
                let off = rng.below(11) as i64 - 5;
                let got = c.incr(g, blog, key.as_bytes(), off).unwrap();
                match (got, model.get(&mk).cloned()) {
                    (Some(n), Some((mt, mv))) => {
                        assert_eq!(n, incr(mt, &mv, off), "step {step}: incr");
                        let (t, b) = n.encode();
                        model.insert(mk, (t, b.to_vec()));
                    }
                    (Some(_), None) => panic!("step {step}: incr of a key the model lacks"),
                    (None, Some(_)) => {
                        assert!(!exact, "step {step}: incr missed a stored key");
                        model.remove(&mk);
                    }
                    (None, None) => {}
                }
            }
            97..=98 => {
                c.flush_group(g).unwrap();
                model.retain(|k, _| k.0 != gi);
            }
            _ => {
                c.flush_namespace(b"model").unwrap();
                model.clear();
            }
        }
        if step % 10_000 == 0 {
            assert_eq!(c.verify(false).unwrap(), vec![], "step {step}");
        }
    }
    assert_eq!(c.verify(false).unwrap(), vec![]);
}

#[test]
fn matches_reference_model_without_eviction() {
    let cfg = Config {
        memory: 64 << 20,
        shards: 8,
        ..small()
    };
    let c = Cache::anonymous(&cfg).unwrap();
    run_model(&c, 0x9E37_79B9, 200_000, true, 3000);
}

#[test]
fn never_returns_stale_values_under_eviction() {
    for policy in [Policy::Lru, Policy::TinyLfu] {
        let cfg = Config {
            memory: 2 << 20,
            shards: 2,
            policy,
            max_item_size: 256 << 10,
            ..small()
        };
        let c = Cache::anonymous(&cfg).unwrap();
        run_model(&c, 0xDEAD_BEEF, 200_000, false, 40_000);
        assert!(c.stats().evictions > 0, "{policy:?}: the test must evict");
    }
}

#[test]
fn survives_random_memory_corruption() {
    let cfg = Config {
        memory: 4 << 20,
        shards: 4,
        ..small()
    };
    let c = Cache::anonymous(&cfg).unwrap();
    let groups: Vec<GroupHandle> = (0..8)
        .map(|i| c.group(b"fuzz", format!("g{i}").as_bytes()))
        .collect();
    let mut rng = Rng(0x1234_5678_9ABC);
    let mut out = Vec::new();
    for round in 0..400 {
        // Scribble: sometimes the allocator/LRU state, mostly buckets and heap.
        let shard = rng.below(4) as usize;
        let state = rng.below(5) == 0;
        let mut junk = vec![0u8; 1 + rng.below(16) as usize];
        for b in &mut junk {
            *b = rng.next() as u8;
        }
        c.test_scribble(shard, state, rng.next() as usize, &junk);
        // Keep using the cache: errors are fine, crashes and hangs are not.
        for _ in 0..300 {
            let g = &groups[rng.below(8) as usize];
            let key = format!("k{}", rng.below(2000));
            match rng.below(4) {
                0 | 1 => {
                    let _ = c.get(g, 0, key.as_bytes(), &mut out);
                }
                2 => {
                    let v = vec![7u8; rng.below(3000) as usize];
                    let _ = c.set(g, 0, key.as_bytes(), TAG_STRING, &v, 0, SetMode::Set);
                }
                _ => {
                    let _ = c.delete(g, 0, key.as_bytes());
                }
            }
        }
        if round % 20 == 0 {
            c.verify(true).unwrap();
            assert_eq!(
                c.verify(false).unwrap(),
                vec![],
                "round {round}: repair left damage"
            );
        }
    }
    // After a repair the cache is fully usable again.
    c.verify(true).unwrap();
    let g = &groups[0];
    for i in 0..500 {
        c.set(
            g,
            0,
            format!("after{i}").as_bytes(),
            TAG_STRING,
            b"ok",
            0,
            SetMode::Set,
        )
        .unwrap();
    }
    for i in 0..500 {
        assert_eq!(
            c.get(g, 0, format!("after{i}").as_bytes(), &mut out)
                .unwrap(),
            Some(TAG_STRING)
        );
    }
    assert!(
        c.stats().recoveries > 0,
        "the corruption was never detected"
    );
}
