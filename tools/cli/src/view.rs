//! How per-group usage is presented.
//!
//! WooCommerce creates one cache group per product (`product_4428`) and
//! similar per-object groups; listed one by one they bury everything else.
//! Groups whose names differ only in their numbers are shown as one family
//! (`product_*`) with the number of groups it covers.

use std::collections::HashMap;
use wprc_core::GroupUsage;

pub struct Row {
    pub namespace: String,
    pub name: String,
    /// Groups folded into this row (1 for a plain group).
    pub groups: u64,
    pub entries: u64,
    pub bytes: u64,
    pub stale: u64,
}

/// `product_4428` → `product_*`; `wc_cache_0.63 1790468959_1` →
/// `wc_cache_* *_*`. Every run of digits (dots included) becomes one `*`.
///
/// A name the directory truncated (ending in `…`) loses its last, partial
/// token first: `wc_cache_0.1 17_b…` and `wc_cache_0.2 18_1…` are the same
/// family, cut at different characters.
pub fn family(name: &str) -> String {
    let name = match name.strip_suffix('…') {
        Some(head) => match head.rfind(['_', ' ', '-']) {
            Some(i) => format!("{}…", &head[..=i]),
            None => name.to_string(),
        },
        None => name.to_string(),
    };
    let mut out = String::with_capacity(name.len());
    let mut in_number = false;
    for ch in name.chars() {
        if ch.is_ascii_digit() || (in_number && ch == '.') {
            if !in_number {
                out.push('*');
                in_number = true;
            }
        } else {
            in_number = false;
            out.push(ch);
        }
    }
    out
}

/// Folds numbered groups sharing a family (at least two of them) into one
/// row; sorted by memory, largest first.
pub fn aggregate(groups: &[GroupUsage]) -> Vec<Row> {
    let mut members: HashMap<(&str, String), u64> = HashMap::new();
    for g in groups {
        *members
            .entry((g.namespace.as_str(), family(&g.name)))
            .or_default() += 1;
    }
    let mut rows: HashMap<(String, String), Row> = HashMap::new();
    for g in groups {
        let fam = family(&g.name);
        let name = if fam != g.name && members[&(g.namespace.as_str(), fam.clone())] >= 2 {
            fam
        } else {
            g.name.clone()
        };
        let row = rows
            .entry((g.namespace.clone(), name.clone()))
            .or_insert_with(|| Row {
                namespace: g.namespace.clone(),
                name,
                groups: 0,
                entries: 0,
                bytes: 0,
                stale: 0,
            });
        row.groups += 1;
        row.entries += g.entries;
        row.bytes += g.bytes;
        row.stale += g.stale_entries;
    }
    let mut out: Vec<Row> = rows.into_values().collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(ns: &str, name: &str, bytes: u64) -> GroupUsage {
        GroupUsage {
            namespace: ns.into(),
            name: name.into(),
            entries: 1,
            bytes,
            stale_entries: 0,
            stale_bytes: 0,
        }
    }

    #[test]
    fn families() {
        assert_eq!(family("product_4428"), "product_*");
        assert_eq!(family("object_19379"), "object_*");
        assert_eq!(
            family("wc_cache_0.63673000 1790468959_1…"),
            "wc_cache_* *_…"
        );
        assert_eq!(
            family("wc_cache_0.12828500 1790468567_b…"),
            "wc_cache_* *_…",
            "names truncated at different characters fold together"
        );
        assert_eq!(family("post_meta"), "post_meta");
        assert_eq!(
            family("pa_ubicazione_relationships"),
            "pa_ubicazione_relationships"
        );
    }

    #[test]
    fn folds_numbered_groups_only_when_there_are_several() {
        let rows = aggregate(&[
            g("site", "terms", 4_000_000),
            g("site", "product_1", 136),
            g("site", "product_2", 136),
            g("site", "product_3", 160),
            g("site", "product_cat", 128),
            g("site", "user_7", 50), // alone: keeps its name
            g("other", "product_9", 136),
        ]);
        let find = |ns: &str, n: &str| rows.iter().find(|r| r.namespace == ns && r.name == n);
        assert_eq!(rows[0].name, "terms");
        let p = find("site", "product_*").unwrap();
        assert_eq!((p.groups, p.entries, p.bytes), (3, 3, 432));
        assert!(find("site", "product_cat").is_some());
        assert!(find("site", "user_7").is_some());
        assert!(
            find("other", "product_9").is_some(),
            "families do not cross namespaces"
        );
    }
}
