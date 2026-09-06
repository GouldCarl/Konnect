//! Netlist partition comparison — the "identical pin partition" gate ported
//! into a first-class tool.
//!
//! `sch_layout_check.py` and `run_erc` both operate on one schematic at a
//! time; the connectivity gate that used to catch BoatDash #14 (a
//! `connect_pins` short ERC never flagged) compares *two* kicad-cli
//! `kicadsexpr` netlists and asks whether they partition component pins into
//! nets the same way. Net **names** are not part of that question — an
//! unlabelled net is named after whichever pin KiCad's netlister happens to
//! pick, so the same physical net can carry a different name across two
//! otherwise-identical exports. What must not change between a layout
//! tidy-up and its baseline is which pins share a net.
//!
//! This module ports the semantics of BoatDash's
//! `hardware/io-expander/tools/netlist_diff.py` (read, not imported): two
//! netlists are "identical" exactly when every net's pin set (ignoring name)
//! in one file also appears, verbatim, in the other. When they differ, this
//! module goes further than the script (which only prints the mismatched
//! nets) and explains *how* pins moved: a net that split across several nets,
//! several nets that merged into one, individual pins that moved to an
//! unrelated net, and pins present in only one of the two files at all.

use konnect_sexp::parser::parse_sexp;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// One net as parsed from a kicadsexpr netlist export: its own name (carried
/// only for readable output — never used to decide equality) and the set of
/// `REF.PIN` members that share it. Nets with no pins are dropped, matching
/// `netlist_diff.py`'s `load()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNet {
    pub name: String,
    pub pins: BTreeSet<String>,
}

/// Parse the `(nets (net (code …) (name …) (node (ref …) (pin …)) …) …)`
/// section of a kicad-cli `kicadsexpr` netlist export.
///
/// Returns a descriptive error (never a panic) for anything that isn't a
/// parseable netlist: a syntax error, or valid S-expression content with no
/// `(nets …)` section (e.g. a `.kicad_sch` file handed to this by mistake).
pub fn parse_netlist(text: &str) -> Result<Vec<ParsedNet>, String> {
    let root = parse_sexp(text).map_err(|error| format!("parse error: {error}"))?;
    let nets_node = root.find("nets").ok_or_else(|| {
        "no (nets ...) section found — not a kicadsexpr netlist export".to_string()
    })?;

    let mut nets = Vec::new();
    for net in nets_node.find_all("net") {
        let name = net.find_str("name").unwrap_or_default().to_string();
        let pins: BTreeSet<String> = net
            .find_all("node")
            .into_iter()
            .filter_map(|node| {
                let reference = node.find_str("ref")?;
                let pin = node.find_str("pin")?;
                Some(format!("{reference}.{pin}"))
            })
            .collect();
        if !pins.is_empty() {
            nets.push(ParsedNet { name, pins });
        }
    }
    Ok(nets)
}

/// A single net's pins redistributed across several nets on the other side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplitEntry {
    pub from: String,
    pub to: Vec<SplitTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplitTarget {
    pub net: String,
    pub pins: Vec<String>,
}

/// Several nets whose pins consolidated onto one net on the other side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MergeEntry {
    pub to: String,
    pub from: Vec<MergeSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MergeSource {
    pub net: String,
    pub pins: Vec<String>,
}

/// One pin that moved to an unrelated net — the general case a clean split
/// or merge doesn't explain (e.g. one pin reassigned while both its old and
/// new nets otherwise stayed put).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MovedPin {
    pub pin: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NetlistDiffDetails {
    pub split: Vec<SplitEntry>,
    pub merged: Vec<MergeEntry>,
    pub moved: Vec<MovedPin>,
    /// Pins present in netlist A but absent from netlist B entirely.
    pub only_in_a: Vec<String>,
    /// Pins present in netlist B but absent from netlist A entirely.
    pub only_in_b: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetlistComparison {
    pub identical: bool,
    pub net_count_a: usize,
    pub net_count_b: usize,
    /// Pins present in both files whose net assignment differs — the split,
    /// merged and moved pins combined, but not the only-one-side pins (those
    /// did not move; they appeared or vanished).
    pub pins_moved_count: usize,
    pub details: NetlistDiffDetails,
}

/// Minimal union-find over a fixed-size universe of local indices.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Compare two parsed netlists by pin partition, per this module's docs.
pub fn diff_partitions(a: &[ParsedNet], b: &[ParsedNet]) -> NetlistComparison {
    let net_count_a = a.len();
    let net_count_b = b.len();

    // Exact pinset equality, names ignored — netlist_diff.py's whole test.
    let a_pinsets: HashSet<&BTreeSet<String>> = a.iter().map(|n| &n.pins).collect();
    let b_pinsets: HashSet<&BTreeSet<String>> = b.iter().map(|n| &n.pins).collect();

    let only_a_nets: Vec<usize> = (0..a.len())
        .filter(|&i| !b_pinsets.contains(&a[i].pins))
        .collect();
    let only_b_nets: Vec<usize> = (0..b.len())
        .filter(|&i| !a_pinsets.contains(&b[i].pins))
        .collect();

    if only_a_nets.is_empty() && only_b_nets.is_empty() {
        return NetlistComparison {
            identical: true,
            net_count_a,
            net_count_b,
            pins_moved_count: 0,
            details: NetlistDiffDetails::default(),
        };
    }

    // Every pin's net index in its own file (not just the "only" nets — an
    // affected pin's home net on the *other* side may still be a fully
    // common net, and we need that to tell only-in-one-side pins apart from
    // moved ones).
    let a_pin_net: HashMap<&str, usize> = a
        .iter()
        .enumerate()
        .flat_map(|(i, net)| net.pins.iter().map(move |p| (p.as_str(), i)))
        .collect();
    let b_pin_net: HashMap<&str, usize> = b
        .iter()
        .enumerate()
        .flat_map(|(i, net)| net.pins.iter().map(move |p| (p.as_str(), i)))
        .collect();

    // The universe of pins touched by any differing net, on either side.
    let mut affected: BTreeSet<&str> = BTreeSet::new();
    for &i in &only_a_nets {
        affected.extend(a[i].pins.iter().map(String::as_str));
    }
    for &i in &only_b_nets {
        affected.extend(b[i].pins.iter().map(String::as_str));
    }
    let pins: Vec<&str> = affected.into_iter().collect();

    let mut only_in_a_pins = Vec::new();
    let mut only_in_b_pins = Vec::new();
    for &p in &pins {
        match (a_pin_net.contains_key(p), b_pin_net.contains_key(p)) {
            (true, false) => only_in_a_pins.push(p.to_string()),
            (false, true) => only_in_b_pins.push(p.to_string()),
            (true, true) => {} // classified below, once nets are grouped into components
            (false, false) => unreachable!("every affected pin came from a real net on one side"),
        }
    }
    only_in_a_pins.sort();
    only_in_b_pins.sort();

    // Union pins that share a net on *either* side — that is exactly the set
    // of nets whose story has to be told together.
    let mut uf = UnionFind::new(pins.len());
    let mut group_by_a: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut group_by_b: HashMap<usize, Vec<usize>> = HashMap::new();
    for (local, &p) in pins.iter().enumerate() {
        if let Some(&net_index) = a_pin_net.get(p) {
            group_by_a.entry(net_index).or_default().push(local);
        }
        if let Some(&net_index) = b_pin_net.get(p) {
            group_by_b.entry(net_index).or_default().push(local);
        }
    }
    for group in group_by_a.values().chain(group_by_b.values()) {
        for pair in group.windows(2) {
            uf.union(pair[0], pair[1]);
        }
    }

    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for local in 0..pins.len() {
        components.entry(uf.find(local)).or_default().push(local);
    }

    let mut split = Vec::new();
    let mut merged = Vec::new();
    let mut moved = Vec::new();

    for idxs in components.values() {
        let mut a_nets_here: BTreeSet<usize> = BTreeSet::new();
        let mut b_nets_here: BTreeSet<usize> = BTreeSet::new();
        for &local in idxs {
            let p = pins[local];
            if let Some(&ni) = a_pin_net.get(p) {
                a_nets_here.insert(ni);
            }
            if let Some(&ni) = b_pin_net.get(p) {
                b_nets_here.insert(ni);
            }
        }

        match (a_nets_here.len(), b_nets_here.len()) {
            (1, n) if n > 1 => {
                let from_idx = *a_nets_here.iter().next().expect("len 1");
                let mut by_b: BTreeMap<usize, Vec<String>> = BTreeMap::new();
                for &local in idxs {
                    let p = pins[local];
                    if let (Some(&ai), Some(&bi)) = (a_pin_net.get(p), b_pin_net.get(p)) {
                        if ai == from_idx {
                            by_b.entry(bi).or_default().push(p.to_string());
                        }
                    }
                }
                let mut to: Vec<SplitTarget> = by_b
                    .into_iter()
                    .map(|(bi, mut ps)| {
                        ps.sort();
                        SplitTarget {
                            net: b[bi].name.clone(),
                            pins: ps,
                        }
                    })
                    .collect();
                to.sort_by(|x, y| x.net.cmp(&y.net));
                split.push(SplitEntry {
                    from: a[from_idx].name.clone(),
                    to,
                });
            }
            (n, 1) if n > 1 => {
                let to_idx = *b_nets_here.iter().next().expect("len 1");
                let mut by_a: BTreeMap<usize, Vec<String>> = BTreeMap::new();
                for &local in idxs {
                    let p = pins[local];
                    if let (Some(&ai), Some(&bi)) = (a_pin_net.get(p), b_pin_net.get(p)) {
                        if bi == to_idx {
                            by_a.entry(ai).or_default().push(p.to_string());
                        }
                    }
                }
                let mut from: Vec<MergeSource> = by_a
                    .into_iter()
                    .map(|(ai, mut ps)| {
                        ps.sort();
                        MergeSource {
                            net: a[ai].name.clone(),
                            pins: ps,
                        }
                    })
                    .collect();
                from.sort_by(|x, y| x.net.cmp(&y.net));
                merged.push(MergeEntry {
                    to: b[to_idx].name.clone(),
                    from,
                });
            }
            (1, 1) => {
                // The one A-net and the one B-net here differ only by which
                // pins are only-in-a / only-in-b, already reported above —
                // there is nothing left to say about the common pins: they
                // did not move, their net simply gained or lost a member.
            }
            _ => {
                // A tangle of 2+ nets on each side with no clean split/merge
                // shape. Pick each A-net's majority destination B-net (the
                // one it shares the most common pins with) as "the same
                // net, renamed"; anything landing elsewhere is a moved pin.
                let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
                for &local in idxs {
                    let p = pins[local];
                    if let (Some(&ai), Some(&bi)) = (a_pin_net.get(p), b_pin_net.get(p)) {
                        *counts.entry((ai, bi)).or_default() += 1;
                    }
                }
                let mut majority: HashMap<usize, usize> = HashMap::new();
                for &ai in &a_nets_here {
                    let mut best: Option<(usize, usize)> = None; // (b_index, count)
                    for (&(a_index, b_index), &count) in counts.iter() {
                        if a_index != ai {
                            continue;
                        }
                        let is_better = match best {
                            None => true,
                            Some((best_bi, best_count)) => {
                                count > best_count || (count == best_count && b_index < best_bi)
                            }
                        };
                        if is_better {
                            best = Some((b_index, count));
                        }
                    }
                    if let Some((bi, _)) = best {
                        majority.insert(ai, bi);
                    }
                }
                for &local in idxs {
                    let p = pins[local];
                    if let (Some(&ai), Some(&bi)) = (a_pin_net.get(p), b_pin_net.get(p)) {
                        if majority.get(&ai) != Some(&bi) {
                            moved.push(MovedPin {
                                pin: p.to_string(),
                                from: a[ai].name.clone(),
                                to: b[bi].name.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    split.sort_by(|x, y| x.from.cmp(&y.from));
    merged.sort_by(|x, y| x.to.cmp(&y.to));
    moved.sort_by(|x, y| x.pin.cmp(&y.pin));

    // A pin "moved" if its net neighbourhood changed at all: every pin swept
    // into a split or merge (its old net no longer exists as such) plus every
    // pin reported individually. Bystanders of a net that merely gained or
    // lost an unrelated member (the (1, 1) case above) are not counted here —
    // they show up in only_in_a / only_in_b instead.
    let pins_moved_count = split
        .iter()
        .map(|e| e.to.iter().map(|t| t.pins.len()).sum::<usize>())
        .sum::<usize>()
        + merged
            .iter()
            .map(|e| e.from.iter().map(|s| s.pins.len()).sum::<usize>())
            .sum::<usize>()
        + moved.len();

    NetlistComparison {
        identical: false,
        net_count_a,
        net_count_b,
        pins_moved_count,
        details: NetlistDiffDetails {
            split,
            merged,
            moved,
            only_in_a: only_in_a_pins,
            only_in_b: only_in_b_pins,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(name: &str, pins: &[&str]) -> ParsedNet {
        ParsedNet {
            name: name.to_string(),
            pins: pins.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn netlist_text(nets: &[(&str, &[(&str, &str)])]) -> String {
        let mut body = String::from("(export\n\t(version \"E\")\n\t(nets\n");
        for (index, (name, nodes)) in nets.iter().enumerate() {
            body.push_str(&format!(
                "\t\t(net (code \"{}\") (name \"{}\")\n",
                index + 1,
                name
            ));
            for (reference, pin) in *nodes {
                body.push_str(&format!(
                    "\t\t\t(node (ref \"{reference}\") (pin \"{pin}\"))\n"
                ));
            }
            body.push_str("\t\t)\n");
        }
        body.push_str("\t)\n)\n");
        body
    }

    #[test]
    fn parses_a_real_shaped_netlist() {
        let text = netlist_text(&[
            ("GND", &[("R1", "2"), ("C1", "1")]),
            ("+3V3", &[("R1", "1")]),
        ]);
        let nets = parse_netlist(&text).unwrap();
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0].name, "GND");
        assert_eq!(
            nets[0].pins,
            BTreeSet::from(["R1.2".to_string(), "C1.1".to_string()])
        );
    }

    #[test]
    fn a_net_with_no_pins_is_dropped() {
        let text = netlist_text(&[("N$1", &[]), ("GND", &[("R1", "1")])]);
        let nets = parse_netlist(&text).unwrap();
        assert_eq!(nets.len(), 1);
        assert_eq!(nets[0].name, "GND");
    }

    #[test]
    fn garbage_text_is_an_explicit_error_not_a_panic() {
        // An unterminated string trips the parser itself, not just the
        // "no (nets ...)" section check below.
        let error = parse_netlist("(export (nets (net (name \"unterminated").unwrap_err();
        assert!(error.contains("parse error"), "{error}");
    }

    #[test]
    fn valid_sexp_without_a_nets_section_is_an_explicit_error() {
        let error = parse_netlist("(kicad_sch (version 1))").unwrap_err();
        assert!(error.contains("no (nets"), "{error}");
    }

    #[test]
    fn identical_partitions_report_identical_true() {
        let a = vec![net("GND", &["R1.1", "R1.2"]), net("+3V3", &["C1.1"])];
        // Same partition, different names and net order — must not matter.
        let b = vec![net("VCC", &["C1.1"]), net("Net-1", &["R1.2", "R1.1"])];
        let diff = diff_partitions(&a, &b);
        assert!(diff.identical);
        assert_eq!(diff.net_count_a, 2);
        assert_eq!(diff.net_count_b, 2);
        assert_eq!(diff.pins_moved_count, 0);
        assert_eq!(diff.details, NetlistDiffDetails::default());
    }

    #[test]
    fn one_pin_moved_between_two_otherwise_stable_nets_is_named() {
        // R2.1 moves from NET1 to NET2; everything else on both nets stays.
        let a = vec![
            net("NET1", &["R2.1", "R1.1", "R1.2"]),
            net("NET2", &["R3.1", "R3.2"]),
        ];
        let b = vec![
            net("NET1", &["R1.1", "R1.2"]),
            net("NET2", &["R3.1", "R3.2", "R2.1"]),
        ];
        let diff = diff_partitions(&a, &b);
        assert!(!diff.identical);
        assert_eq!(diff.pins_moved_count, 1);
        assert_eq!(diff.details.split, vec![]);
        assert_eq!(diff.details.merged, vec![]);
        assert_eq!(
            diff.details.moved,
            vec![MovedPin {
                pin: "R2.1".to_string(),
                from: "NET1".to_string(),
                to: "NET2".to_string(),
            }]
        );
        assert!(diff.details.only_in_a.is_empty());
        assert!(diff.details.only_in_b.is_empty());
    }

    #[test]
    fn a_net_that_splits_into_two_is_reported_as_a_split() {
        let a = vec![net("BUS", &["R1.1", "R2.1", "R3.1"])];
        let b = vec![
            net("BUS", &["R1.1"]),
            net("NEW_A", &["R2.1"]),
            net("NEW_B", &["R3.1"]),
        ];
        let diff = diff_partitions(&a, &b);
        assert!(!diff.identical);
        assert_eq!(diff.pins_moved_count, 3);
        assert_eq!(diff.details.merged, vec![]);
        assert_eq!(diff.details.moved, vec![]);
        assert_eq!(diff.details.split.len(), 1);
        let entry = &diff.details.split[0];
        assert_eq!(entry.from, "BUS");
        assert_eq!(
            entry.to,
            vec![
                SplitTarget {
                    net: "BUS".to_string(),
                    pins: vec!["R1.1".to_string()],
                },
                SplitTarget {
                    net: "NEW_A".to_string(),
                    pins: vec!["R2.1".to_string()],
                },
                SplitTarget {
                    net: "NEW_B".to_string(),
                    pins: vec!["R3.1".to_string()],
                },
            ]
        );
    }

    #[test]
    fn two_nets_that_merge_into_one_are_reported_as_a_merge() {
        let a = vec![net("NET_A", &["R1.1"]), net("NET_B", &["R2.1"])];
        let b = vec![net("MERGED", &["R1.1", "R2.1"])];
        let diff = diff_partitions(&a, &b);
        assert!(!diff.identical);
        assert_eq!(diff.pins_moved_count, 2);
        assert_eq!(diff.details.split, vec![]);
        assert_eq!(diff.details.moved, vec![]);
        assert_eq!(diff.details.merged.len(), 1);
        let entry = &diff.details.merged[0];
        assert_eq!(entry.to, "MERGED");
        assert_eq!(
            entry.from,
            vec![
                MergeSource {
                    net: "NET_A".to_string(),
                    pins: vec!["R1.1".to_string()],
                },
                MergeSource {
                    net: "NET_B".to_string(),
                    pins: vec!["R2.1".to_string()],
                },
            ]
        );
    }

    #[test]
    fn a_pin_missing_entirely_from_one_side_is_only_in_that_side() {
        let a = vec![net("GND", &["R1.1", "R1.2", "C1.1"])];
        // C1 removed outright; the rest of GND is unchanged.
        let b = vec![net("GND", &["R1.1", "R1.2"])];
        let diff = diff_partitions(&a, &b);
        assert!(!diff.identical);
        assert_eq!(diff.pins_moved_count, 0);
        assert_eq!(diff.details.split, vec![]);
        assert_eq!(diff.details.merged, vec![]);
        assert_eq!(diff.details.moved, vec![]);
        assert_eq!(diff.details.only_in_a, vec!["C1.1".to_string()]);
        assert!(diff.details.only_in_b.is_empty());
    }

    #[test]
    fn a_new_component_pin_with_no_history_is_only_in_b() {
        let a = vec![net("GND", &["R1.1"])];
        let b = vec![net("GND", &["R1.1", "C9.1"])];
        let diff = diff_partitions(&a, &b);
        assert!(!diff.identical);
        assert_eq!(diff.details.only_in_b, vec!["C9.1".to_string()]);
        assert!(diff.details.only_in_a.is_empty());
    }

    #[test]
    fn a_no_op_edit_leaves_the_partition_untouched() {
        // Moving a symbol with nothing attached changes no REF.PIN membership.
        let a = vec![net("GND", &["R1.1", "R1.2"])];
        let b = vec![net("GND", &["R1.1", "R1.2"])];
        assert!(diff_partitions(&a, &b).identical);
    }
}
