//! The CDC analysis engine.
//!
//! Builds a net→driver graph, assigns each flop a clock domain (trace its clock
//! pin back to an SDC clock source), walks each capture flop's data cone back to
//! its launching flops, and reports every cross-domain launch→capture pair —
//! classifying the canonical two-flop synchronizer.

use std::collections::{BTreeMap, BTreeSet};

use crate::liberty::{Dir, Lib};
use crate::names::{split_bit_select, split_inst_pin};
use crate::netlist::{Inst, Netlist};
use crate::sdc::{ClockGroups, Sdc};
use vyges_loom::sdc_clock_relations::Families;
use vyges_loom::sdc_path_fate::{setup_fate, Fate, RegPath, Through};

#[derive(Debug, Clone)]
pub struct Crossing {
    pub from_flop: String,
    pub from_domain: String,
    pub to_flop: String,
    pub to_domain: String,
    /// The crossing path runs through combinational logic (a synchronizer's first
    /// stage must sample the source directly — logic on a CDC path is a red flag).
    pub through_logic: bool,
    /// Recognized as a clean two-flop synchronizer (direct Q→D, second stage present).
    pub synchronized: bool,
    /// What the timer does with this path's setup check, as the reference decides it
    /// ([`vyges_loom::sdc_path_fate`]): untimed (clock groups, a false path), timed against a max
    /// delay, a multicycle or the clocks' edges, or not decided here. An asynchronous crossing
    /// timed against the clocks' edges is checked against an edge relation the clocks do not
    /// have: the SDC says nothing about it.
    pub setup: Fate,
}

/// Bits of one bus crossing the same domain pair, **each through its own synchronizer**.
///
/// Every bit is individually safe and every bit reports `OK`. The bus is not safe: the
/// synchronizers resolve independently, so two bits can settle on different capture edges and
/// the receiver latches a combination that never existed at the source. A counter crossing as
/// `0111 → 1000` can be read as `1111`.
///
/// The check cannot tell this apart from a **correctly** designed multi-bit crossing, because
/// what makes one safe is not structural: gray coding (only one bit changes per transition) and
/// handshake qualification (the receiver only samples while the bus is stable) are properties of
/// the data and the protocol, not of the netlist. So this is reported as something to look at,
/// not as a proven defect — and it is deliberately not part of the `--fail-on-violation` gate
/// (see `--fail-on-multibit`).
#[derive(Debug, Clone)]
pub struct MultiBitCrossing {
    pub from_domain: String,
    pub to_domain: String,
    /// Base name of the launching flops, bit-select stripped (`core/data_reg`).
    pub bus_from: String,
    /// Base name of the capturing (first-stage) flops.
    pub bus_to: String,
    /// The launch bit indices seen crossing, sorted.
    pub bits: Vec<i64>,
}

impl MultiBitCrossing {
    /// `data_reg[3:0]`-style span of the bits that cross.
    pub fn bit_span(&self) -> String {
        match (self.bits.first(), self.bits.last()) {
            (Some(lo), Some(hi)) if self.bits.len() as i64 == hi - lo + 1 => format!("[{hi}:{lo}]"),
            _ => format!(
                "[{}]",
                self.bits
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct CdcReport {
    pub crossings: Vec<Crossing>,
    /// Buses whose bits are each synchronized but which cross as a group. Every one of these
    /// is reported `OK` bit by bit in [`crossings`](CdcReport::crossings) — that is the point.
    pub multibit: Vec<MultiBitCrossing>,
    /// Flop instance -> the declared clock domain(s) reaching its clock pin, sorted. More than
    /// one means a clock mux.
    pub flop_domain: BTreeMap<String, Vec<String>>,
    pub domains: Vec<String>,
    /// Every sequential instance in the netlist, whether or not it could be placed.
    pub flops_total: usize,
    /// Flops whose clock did not trace back to a declared clock, **in netlist order**.
    ///
    /// These are excluded from the analysis, and that exclusion is the report's most important
    /// disclosure: a crossing into or out of one of them cannot be seen, so "no crossings" over
    /// a partly-placed design means something weaker than it appears to. Naming them is the
    /// difference between "I looked and it was clean" and "I could not look at all of it".
    pub unplaced: Vec<String>,
    /// Flops reachable from more than one declared clock — a clock mux. Each is analysed in
    /// **every** domain that reaches it, so both sides of the mux are checked; the list is here
    /// because a muxed clock is a design construct worth knowing about (whether the mux is
    /// glitch-free is a question this check does not ask).
    pub multi_clocked: Vec<String>,
    /// Launch→capture pairs in *different* domains that the SDC declares **related** to each
    /// other, so they are timed rather than crossed. Counted, not reported as crossings —
    /// a number here says the clock grouping was read and applied.
    pub related_skipped: usize,
    /// Launch→capture pairs in two clocks of ONE family (a generated clock and its master, or two
    /// generated from one clock), which no `set_clock_groups` separates: synchronous by
    /// construction, timed by the timer, not crossed. Counted, like `related_skipped`.
    pub synchronous_skipped: usize,
}

/// What drives a net.
struct Driver {
    inst: Option<usize>, // None = primary input port
    is_seq: bool,
}

fn is_in(lib: &Lib, cell: &str, pin: &str) -> bool {
    lib.cells
        .get(cell)
        .and_then(|c| c.pins.get(pin))
        .map(|p| p.direction)
        == Some(Dir::In)
}

fn net_of<'a>(inst: &'a Inst, pin: &str) -> Option<&'a str> {
    inst.conns
        .iter()
        .find(|(p, _)| p == pin)
        .map(|(_, n)| n.as_str())
}

/// `(clock_pin, data_pins, q_pins)` for a sequential cell, else `None`.
fn flop_pins(lib: &Lib, cell: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let c = lib.cells.get(cell)?;
    if !c.is_seq {
        return None;
    }
    let clk = c.clock_pin.clone()?;
    let d = c
        .pins
        .iter()
        .filter(|(_, p)| !p.setup.is_empty() || !p.hold.is_empty())
        .map(|(n, _)| n.clone())
        .collect();
    let q = c
        .pins
        .iter()
        .filter(|(_, p)| p.direction == Dir::Out)
        .map(|(n, _)| n.clone())
        .collect();
    Some((clk, d, q))
}

/// Trace a clock net back through combinational clock cells to **every** SDC clock source that
/// reaches it.
///
/// Every one, not the first. A clock mux has two clocks arriving at one flop and both are real:
/// returning whichever the netlist happened to wire first missed the other crossing entirely,
/// and made the verdict depend on connection order — the same design reported 0 or 1 crossings
/// depending on how the synthesiser wrote the mux instance. A set, walked in sorted order, is
/// also the only version of this that is deterministic.
///
/// Recursion stops at a declared clock source (it is the boundary), at a cycle, and at a
/// sequential cell (a divided clock off a flop, the v0 limit stated in the README).
fn trace_clocks(
    net: &str,
    nd: &BTreeMap<String, Driver>,
    nl: &Netlist,
    lib: &Lib,
    src: &BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    if let Some(d) = src.get(net) {
        out.insert(d.clone());
        return;
    }
    if !seen.insert(net.to_string()) {
        return;
    }
    let Some(drv) = nd.get(net) else { return };
    let Some(i) = drv.inst else { return }; // a port that isn't an SDC clock source -> unknown
    if drv.is_seq {
        return; // divided/gated clock off a flop — not modelled in v0
    }
    let inst = &nl.insts[i];
    for (pin, n) in &inst.conns {
        if is_in(lib, &inst.cell, pin) {
            trace_clocks(n, nd, nl, lib, src, seen, out);
        }
    }
}

/// Walk a data net's combinational cone back to launching flops. Each result is
/// `(flop_inst_index, direct)` where `direct` means the flop's Q drives this cone
/// with no combinational logic in between.
fn launch_flops(
    net: &str,
    direct: bool,
    nd: &BTreeMap<String, Driver>,
    nl: &Netlist,
    lib: &Lib,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<(usize, bool)>,
) {
    if !seen.insert(net.to_string()) {
        return;
    }
    let Some(drv) = nd.get(net) else { return };
    let Some(i) = drv.inst else { return }; // primary input — stop
    if drv.is_seq {
        out.push((i, direct));
        return;
    }
    let inst = &nl.insts[i];
    for (pin, n) in &inst.conns {
        if is_in(lib, &inst.cell, pin) {
            launch_flops(n, false, nd, nl, lib, seen, out); // through a comb cell -> not direct
        }
    }
}

/// The net a clock source names.
///
/// An SDC clock is attached to a port (`clk`) or to a **pin** (`u_div/Q`) — and a generated
/// clock is attached to a pin essentially always, because that is where a divider's output
/// lives. Clock tracing walks *nets*, so a pin-form source has to be resolved to the net that
/// pin drives or it matches nothing: the clock is declared, the domain appears in the report,
/// and not one flop is ever placed in it.
///
/// The last-separator rule this depends on lives in [`crate::names::split_inst_pin`] — a
/// flattened netlist puts hierarchy in the *instance* name, so `core/u_div/Q` is pin `Q` of
/// instance `core/u_div`.
fn source_net<'a>(source: &str, nl: &'a Netlist) -> Option<&'a str> {
    let (inst_name, pin) = split_inst_pin(source)?;
    let inst = nl.insts.iter().find(|i| i.name == inst_name)?;
    net_of(inst, pin)
}

/// How two clock domains relate, for CDC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// A crossing: the SDC separates them, or says nothing that relates them.
    Asynchronous,
    /// A command puts them in one group, and none separates them.
    Related,
    /// One family (one is generated from the other, or both from one clock), and no command
    /// separates them.
    Synchronous,
}

/// How two domains relate.
///
/// `set_clock_groups` is the SDC's own statement about this, read per COMMAND, as the timer
/// reads it ([`vyges_loom::sdc::clocks_unrelated`]): two clocks are asynchronous when any one
/// command puts them in different groups, or is a single group holding one of them. They are
/// related when a command puts them in one group and no command separates them. Two clocks of
/// one family ([`vyges_loom::sdc_clock_relations::families`], each generated clock's master
/// found as the timer finds it) are synchronous unless a command separates them. Otherwise every
/// differently-named clock is a crossing: noisy on a real SoC, but the conservative reading.
///
/// For CDC, `-asynchronous -allow_paths` separates too: the timer still times those paths, but
/// the clocks are declared asynchronous and each crossing needs a synchronizer.
///
/// ⛔ The groups of all commands were once merged into one list and a clock looked up in the
/// first group naming it. A file that groups `{a b}` in one command and separates `a` from `b`
/// in another then read `a` and `b` as related, and the crossing was never reported.
fn relation(a: &str, b: &str, groups: &[ClockGroups], families: &Families) -> Relation {
    if a == b {
        return Relation::Synchronous;
    }
    let declared: Vec<ClockGroups> = groups.iter().map(|g| ClockGroups { allow_paths: false, ..g.clone() }).collect();
    if vyges_loom::sdc::clocks_unrelated(&declared, a, b) {
        return Relation::Asynchronous;
    }
    // An undecided clock is its own family root, and no other clock's chain ends at it, so two
    // different clocks share a root only when both chains reached the same create_clock.
    if families.root.contains_key(a) && families.root.get(a) == families.root.get(b) {
        return Relation::Synchronous;
    }
    let together = groups.iter().any(|g| g.groups.iter().any(|grp| grp.iter().any(|n| n == a) && grp.iter().any(|n| n == b)));
    if together {
        Relation::Related
    } else {
        Relation::Asynchronous
    }
}

/// Group synchronized crossings into the buses they belong to.
///
/// Keyed on `(from_domain, to_domain, launch base, capture base)`: bits of one bus are launched
/// by flops sharing a base name and captured by synchronizer flops sharing one, and a bus that
/// crosses into two different places is two different crossings. Requiring **both** ends to
/// match keeps two unrelated signals that happen to share a launch register file apart.
///
/// Only crossings already reported `OK` are grouped. A bit with no synchronizer is a violation
/// on its own and is reported as one; the finding here is precisely the case that currently
/// reads clean.
///
/// A design whose bus bits are not named `base[i]` — spelled `data0`, `data1` — cannot be
/// grouped, because after synthesis the bit-select suffix is the only evidence left that the
/// bits belong together.
fn multi_bit_crossings(crossings: &[Crossing]) -> Vec<MultiBitCrossing> {
    let mut groups: BTreeMap<(String, String, String, String), BTreeSet<i64>> = BTreeMap::new();
    for c in crossings.iter().filter(|c| c.synchronized) {
        let (bus_from, Some(bit)) = split_bit_select(&c.from_flop) else {
            continue;
        };
        let (bus_to, Some(_)) = split_bit_select(&c.to_flop) else {
            continue;
        };
        groups
            .entry((
                c.from_domain.clone(),
                c.to_domain.clone(),
                bus_from.to_string(),
                bus_to.to_string(),
            ))
            .or_default()
            .insert(bit);
    }
    groups
        .into_iter()
        .filter(|(_, bits)| bits.len() > 1)
        .map(
            |((from_domain, to_domain, bus_from, bus_to), bits)| MultiBitCrossing {
                from_domain,
                to_domain,
                bus_from,
                bus_to,
                bits: bits.into_iter().collect(),
            },
        )
        .collect()
}

pub fn analyze(nl: &Netlist, lib: &Lib, sdc: &Sdc) -> Result<CdcReport, String> {
    if lib.cells.is_empty() {
        return Err("no cells in the Liberty".into());
    }
    // SDC clock source -> domain name, keyed by every spelling that can reach a net: the
    // source as written, and — for a pin-form source — the net that pin drives.
    let mut src: BTreeMap<String, String> = BTreeMap::new();
    for c in &sdc.clocks {
        if c.is_virtual() {
            continue; // constrains I/O timing, launches nothing in this design
        }
        src.insert(c.source.clone(), c.name.clone());
        if let Some(net) = source_net(&c.source, nl) {
            src.insert(net.to_string(), c.name.clone());
        }
    }

    // net -> driver
    let mut nd: BTreeMap<String, Driver> = BTreeMap::new();
    for inp in &nl.inputs {
        nd.insert(
            inp.clone(),
            Driver {
                inst: None,
                is_seq: false,
            },
        );
    }
    for (i, inst) in nl.insts.iter().enumerate() {
        let Some(cell) = lib.cells.get(&inst.cell) else {
            continue;
        };
        for (pin, net) in &inst.conns {
            if cell.pins.get(pin).map(|p| p.direction) == Some(Dir::Out) {
                nd.insert(
                    net.clone(),
                    Driver {
                        inst: Some(i),
                        is_seq: cell.is_seq,
                    },
                );
            }
        }
    }

    // domain(s) per flop instance (trace clock pin)
    let mut flop_domain: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut flops_total = 0usize;
    let mut unplaced: Vec<String> = Vec::new();
    let mut multi_clocked: Vec<String> = Vec::new();
    for inst in &nl.insts {
        let Some((clk, _, _)) = flop_pins(lib, &inst.cell) else {
            continue;
        };
        flops_total += 1;
        let mut doms = BTreeSet::new();
        if let Some(cn) = net_of(inst, &clk) {
            trace_clocks(cn, &nd, nl, lib, &src, &mut BTreeSet::new(), &mut doms);
        }
        if doms.is_empty() {
            // Unplaced: a divided or gated clock off a flop, or a clock port the SDC never
            // declared. Recorded rather than passed over, because everything downstream skips
            // this flop and the report would otherwise look complete.
            unplaced.push(inst.name.clone());
            continue;
        }
        if doms.len() > 1 {
            multi_clocked.push(inst.name.clone());
        }
        flop_domain.insert(inst.name.clone(), doms.into_iter().collect());
    }

    // crossings: for each capture flop, walk its D cone to launch flops
    let mut crossings = Vec::new();
    let mut related_skipped = 0usize;
    let mut synchronous_skipped = 0usize;
    let families = vyges_loom::sdc_clock_relations::families(sdc, Some((nl, lib)));
    for inst in &nl.insts {
        let Some((_, dpins, _)) = flop_pins(lib, &inst.cell) else {
            continue;
        };
        let Some(capture_domains) = flop_domain.get(&inst.name) else {
            continue;
        };
        for d in &dpins {
            let Some(dn) = net_of(inst, d) else { continue };
            let mut launches = Vec::new();
            launch_flops(dn, true, &nd, nl, lib, &mut BTreeSet::new(), &mut launches);
            for (li, direct) in launches {
                let lname = &nl.insts[li].name;
                let Some(launch_domains) = flop_domain.get(lname) else {
                    continue;
                };
                // Every launch/capture domain pair. A muxed flop is genuinely in more than one
                // domain, and each pair that is asynchronous is a separate real crossing — the
                // signal does cross both ways, at different times.
                for dl in launch_domains {
                    for dc in capture_domains {
                        if dl == dc {
                            continue; // same domain, not a crossing
                        }
                        match relation(dl, dc, &sdc.clock_groups, &families) {
                            Relation::Related => {
                                related_skipped += 1; // declared related: timed, not crossed
                                continue;
                            }
                            Relation::Synchronous => {
                                synchronous_skipped += 1; // one family: timed, not crossed
                                continue;
                            }
                            Relation::Asynchronous => {}
                        }
                        let synchronized =
                            direct && has_second_stage(inst, dc, lib, nl, &nd, &flop_domain);
                        let setup = crossing_fate(sdc, nl, lib, li, inst, d, dn, direct, dl, dc);
                        crossings.push(Crossing {
                            from_flop: lname.clone(),
                            from_domain: dl.clone(),
                            to_flop: inst.name.clone(),
                            to_domain: dc.clone(),
                            through_logic: !direct,
                            synchronized,
                            setup,
                        });
                    }
                }
            }
        }
    }

    let mut domains: Vec<String> = sdc.clocks.iter().map(|c| c.name.clone()).collect();
    domains.sort();
    domains.dedup();
    let multibit = multi_bit_crossings(&crossings);
    Ok(CdcReport {
        crossings,
        multibit,
        flop_domain,
        domains,
        flops_total,
        unplaced,
        multi_clocked,
        related_skipped,
        synchronous_skipped,
    })
}

/// The timer's fate for the setup check from launch flop `li` (clock `dl`) to `cap`'s data pin
/// `d` (clock `dc`). A direct path passes exactly the launch output pin driving `dn`, the net
/// `dn` and the data pin; one through logic passes pins this does not list.
#[allow(clippy::too_many_arguments)]
fn crossing_fate(sdc: &Sdc, nl: &Netlist, lib: &Lib, li: usize, cap: &Inst, d: &str, dn: &str, direct: bool, dl: &str, dc: &str) -> Fate {
    let launch = &nl.insts[li];
    let Some((clk, _, _)) = flop_pins(lib, &launch.cell) else {
        return Fate::Undecided(format!("`{}` has no clock pin in the library", launch.name));
    };
    let through = direct.then(|| {
        let outs = launch.conns.iter().filter(|(p, n)| n == dn && !is_in(lib, &launch.cell, p));
        let mut pins: Vec<String> = outs.map(|(p, _)| format!("{}/{p}", launch.name)).collect();
        pins.push(format!("{}/{d}", cap.name));
        Through { pins, nets: vec![dn.to_string()] }
    });
    let path = RegPath {
        launch_inst: &launch.name,
        launch_clock_pin: &clk,
        launch_clock: dl,
        capture_inst: &cap.name,
        capture_data_pin: d,
        capture_clock: dc,
        through,
    };
    setup_fate(sdc, &path)
}

/// Is the capture flop the first stage of a 2-flop synchronizer? — does its Q
/// directly drive the D of another flop in the same domain (no logic between)?
fn has_second_stage(
    cap: &Inst,
    domain: &str,
    lib: &Lib,
    nl: &Netlist,
    nd: &BTreeMap<String, Driver>,
    flop_domain: &BTreeMap<String, Vec<String>>,
) -> bool {
    let Some((_, _, qpins)) = flop_pins(lib, &cap.cell) else {
        return false;
    };
    for q in &qpins {
        let Some(qn) = net_of(cap, q) else { continue };
        for s2 in &nl.insts {
            // The second stage must be clocked by the domain this crossing lands in. With a
            // muxed flop that is membership, not equality — it may sit in several.
            if !flop_domain
                .get(&s2.name)
                .map(|ds| ds.iter().any(|d| d == domain))
                .unwrap_or(false)
            {
                continue;
            }
            let Some((_, d2pins, _)) = flop_pins(lib, &s2.cell) else {
                continue;
            };
            for d2 in &d2pins {
                if net_of(s2, d2) == Some(qn) {
                    // d2 is on cap's Q net; confirm cap.Q is its *direct* driver
                    if let Some(drv) = nd.get(qn) {
                        if drv.inst.map(|i| &nl.insts[i].name) == Some(&cap.name) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Lib {
        Lib::load("examples/cells.lib").expect("cells.lib")
    }
    fn sdc() -> Sdc {
        // two domains on ports clk1 / clk2
        Sdc::parse("create_clock -name clk1 -period 10 [get_ports clk1]\ncreate_clock -name clk2 -period 7 [get_ports clk2]\n").unwrap()
    }

    #[test]
    fn unsynchronized_crossing_is_flagged() {
        // A (clk1) -> B (clk2) directly: a single-flop, no-synchronizer crossing.
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,y);\ninput clk1,clk2; output y;\nwire q;\nDFF a(.CK(clk1),.D(y),.Q(q));\nDFF b(.CK(clk2),.D(q),.Q(y));\nendmodule\n",
        )
        .unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        assert_eq!(r.flop_domain.get("a"), Some(&vec!["clk1".to_string()]));
        assert_eq!(r.flop_domain.get("b"), Some(&vec!["clk2".to_string()]));
        let c: Vec<_> = r.crossings.iter().filter(|c| c.to_flop == "b").collect();
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0].from_domain.as_str(), c[0].to_domain.as_str()),
            ("clk1", "clk2")
        );
        assert!(!c[0].synchronized, "single flop -> not synchronized");
    }

    #[test]
    fn two_flop_synchronizer_is_recognized() {
        // A (clk1) -> S1 (clk2) -> S2 (clk2): a clean 2-DFF synchronizer.
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,y);\ninput clk1,clk2; output y;\nwire a_q,s1_q;\n\
             DFF a(.CK(clk1),.D(y),.Q(a_q));\nDFF s1(.CK(clk2),.D(a_q),.Q(s1_q));\n\
             DFF s2(.CK(clk2),.D(s1_q),.Q(y));\nendmodule\n",
        )
        .unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        let c: Vec<_> = r
            .crossings
            .iter()
            .filter(|c| c.from_flop == "a" && c.to_flop == "s1")
            .collect();
        assert_eq!(c.len(), 1, "the clk1->clk2 crossing into s1");
        assert!(c[0].synchronized, "s1+s2 is a 2-flop synchronizer");
        assert!(!c[0].through_logic);
    }

    /// A divide-by-2 off `clk1` driving a capture flop, fed by a `clk2` flop with no
    /// synchronizer — a textbook violation that lives behind a generated clock.
    const DIV_V: &str = "module t(clk1,clk2,d,y);\ninput clk1,clk2,d; output y;\n\
         wire clk_div,div_n,src_q;\n\
         DFF u_div(.CK(clk1),.D(div_n),.Q(clk_div));\nINV u_dinv(.A(clk_div),.Y(div_n));\n\
         DFF src(.CK(clk2),.D(d),.Q(src_q));\nDFF cap(.CK(clk_div),.D(src_q),.Q(y));\nendmodule\n";

    #[test]
    fn a_generated_clock_declared_on_a_pin_places_its_flops() {
        // THE FORM EVERY REAL SDC USES. A generated clock is attached to the divider's output
        // *pin*; clock tracing walks *nets*. Matching the pin string against net names found
        // nothing, so the domain appeared in the report with no flop in it and the crossing
        // below went unreported — the feature never fired on a real design.
        let nl = crate::netlist::parse(DIV_V).unwrap();
        let sdc = Sdc::parse(
            "create_clock -name clk1 -period 10 [get_ports clk1]\n\
             create_clock -name clk2 -period 7 [get_ports clk2]\n\
             create_generated_clock -name clk_div -source clk1 -divide_by 2 [get_pins u_div/Q]\n",
        )
        .unwrap();
        let r = analyze(&nl, &lib(), &sdc).unwrap();
        assert_eq!(r.flop_domain.get("cap"), Some(&vec!["clk_div".to_string()]));
        assert!(
            r.unplaced.is_empty(),
            "every flop is placed: {:?}",
            r.unplaced
        );
        let c: Vec<_> = r.crossings.iter().filter(|c| c.to_flop == "cap").collect();
        assert_eq!(c.len(), 1, "the clk2 -> clk_div crossing");
        assert!(!c[0].synchronized);
    }

    #[test]
    fn a_flop_whose_clock_does_not_resolve_is_named_not_dropped() {
        // Without the generated clock declared, `cap` genuinely cannot be placed — that is a
        // stated v0 limit. What must not happen is the limit being invisible: before, this
        // reported "0 crossings" with no census, which reads exactly like a clean design.
        let nl = crate::netlist::parse(DIV_V).unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        assert_eq!(r.flops_total, 3);
        assert_eq!(r.unplaced, vec!["cap".to_string()]);
        assert_eq!(r.flop_domain.len(), 2);
        assert!(
            r.crossings.is_empty(),
            "the crossing into cap is not visible"
        );
    }

    #[test]
    fn clocks_the_sdc_declares_related_are_not_crossings() {
        // set_clock_groups is the SDC's own statement about which clocks are asynchronous.
        // Ignoring it reports every synchronous divide off one PLL as a CDC violation, which
        // is the noise that gets a checker switched off.
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,clk3,d,y2,y3);\ninput clk1,clk2,clk3,d; output y2,y3;\nwire q;\n\
             DFF a(.CK(clk1),.D(d),.Q(q));\nDFF b2(.CK(clk2),.D(q),.Q(y2));\n\
             DFF b3(.CK(clk3),.D(q),.Q(y3));\nendmodule\n",
        )
        .unwrap();
        let three = "create_clock -name clk1 -period 10 [get_ports clk1]\n\
                     create_clock -name clk2 -period 10 [get_ports clk2]\n\
                     create_clock -name clk3 -period 7 [get_ports clk3]\n";
        let grouped = Sdc::parse(&format!(
            "{three}set_clock_groups -asynchronous -group {{clk1 clk2}} -group {{clk3}}\n"
        ))
        .unwrap();
        let r = analyze(&nl, &lib(), &grouped).unwrap();
        assert_eq!(r.crossings.len(), 1, "only clk1 -> clk3 is asynchronous");
        assert_eq!(r.crossings[0].to_flop, "b3");
        assert_eq!(r.related_skipped, 1, "clk1 -> clk2 is related, and says so");

        // With no grouping declared, nothing is known and both stay crossings — the
        // conservative reading, and the behaviour before this existed.
        let r = analyze(&nl, &lib(), &Sdc::parse(three).unwrap()).unwrap();
        assert_eq!(r.crossings.len(), 2);
        assert_eq!(r.related_skipped, 0);

        // A SINGLE group states relatedness too, and is the natural way to write it for a
        // design whose clocks are all related. It cuts no timing path, so the shared SDC
        // reader used to discard it and every such pair came back a crossing.
        let one = Sdc::parse(&format!(
            "{three}set_clock_groups -asynchronous -group {{clk1 clk2 clk3}}\n"
        ))
        .unwrap();
        let r = analyze(&nl, &lib(), &one).unwrap();
        assert!(r.crossings.is_empty(), "all three are declared related");
        assert_eq!(r.related_skipped, 2);
    }

    /// Clock groups read per command, as the timer reads them: a command that separates two
    /// clocks makes them asynchronous even when another puts them in one group; `-allow_paths`
    /// still declares them asynchronous (the timer times the paths; CDC still needs a
    /// synchronizer).
    #[test]
    fn a_command_that_separates_two_clocks_makes_a_crossing() {
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,d,y);\ninput clk1,clk2,d; output y;\nwire q;\n\
             DFF a(.CK(clk1),.D(d),.Q(q));\nDFF b(.CK(clk2),.D(q),.Q(y));\nendmodule\n",
        )
        .unwrap();
        let two = "create_clock -name clk1 -period 10 [get_ports clk1]\n\
                   create_clock -name clk2 -period 7 [get_ports clk2]\n";
        let crossings = |extra: &str| analyze(&nl, &lib(), &Sdc::parse(&format!("{two}{extra}")).unwrap()).unwrap().crossings.len();
        assert_eq!(crossings("set_clock_groups -asynchronous -group {clk1 clk2}\n"), 0, "related");
        assert_eq!(
            crossings("set_clock_groups -asynchronous -group {clk1 clk2}\nset_clock_groups -asynchronous -group clk1 -group clk2\n"),
            1,
            "the second command separates them"
        );
        assert_eq!(
            crossings("set_clock_groups -asynchronous -group {clk1 clk2}\nset_clock_groups -asynchronous -allow_paths -group clk1 -group clk2\n"),
            1,
            "-allow_paths declares them asynchronous though another command groups them"
        );
    }

    /// Each crossing says what the timer does with it (the reference's rules, witnessed by
    /// `crossing-fate-probe.py`): untimed under a separating set_clock_groups, timed against a
    /// max delay that wins, against the clocks' edges when the SDC says nothing, and untimed by
    /// a false path through the crossing's own net. Through logic, a -through cannot be seen.
    #[test]
    fn each_crossing_says_whether_the_timer_times_it() {
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,d,y,z);\ninput clk1,clk2,d; output y,z;\nwire q,q2,x;\n\
             DFF a(.CK(clk1),.D(d),.Q(q));\nDFF b(.CK(clk2),.D(q),.Q(y));\n\
             DFF a2(.CK(clk1),.D(d),.Q(q2));\nINV g(.A(q2),.Y(x));\nDFF b2(.CK(clk2),.D(x),.Q(z));\nendmodule\n",
        )
        .unwrap();
        let two = "create_clock -name clk1 -period 10 [get_ports clk1]\n\
                   create_clock -name clk2 -period 7 [get_ports clk2]\n";
        let fates = |extra: &str| -> Vec<(String, Fate)> {
            let r = analyze(&nl, &lib(), &Sdc::parse(&format!("{two}{extra}")).unwrap()).unwrap();
            let mut v: Vec<(String, Fate)> = r.crossings.into_iter().map(|c| (c.to_flop, c.setup)).collect();
            v.sort_by(|x, y| x.0.cmp(&y.0));
            v
        };
        let only = |v: Vec<(String, Fate)>| -> Vec<Fate> { v.into_iter().map(|(_, f)| f).collect() };
        assert_eq!(only(fates("")), [Fate::ClockEdges, Fate::ClockEdges]);
        let g = only(fates("set_clock_groups -asynchronous -name io -group clk1 -group clk2\n"));
        assert_eq!(g, vec![Fate::ClockGroups("set_clock_groups -asynchronous -name io".into()); 2]);
        let d = only(fates("set_max_delay 4 -from [get_clocks clk1] -to [get_clocks clk2]\n"));
        assert!(d.iter().all(|f| matches!(f, Fate::MaxDelay(v, _) if *v == 4.0)), "{d:?}");
        let n = fates("set_false_path -through [get_nets q]\n");
        assert!(matches!(n[0].1, Fate::FalsePath(_)), "b's path passes net q: {n:?}");
        assert!(matches!(n[1].1, Fate::Undecided(_)), "b2's path is through logic: {n:?}");
        let p = fates("set_false_path -through [get_pins a/Q]\n");
        assert!(matches!(p[0].1, Fate::FalsePath(_)), "b's path passes a/Q: {p:?}");
    }

    /// A divide-by-2 off clk1 is clk1's family: a clk1 -> clk_div path is timed, not crossed,
    /// unless the SDC separates the two; clk2 -> clk_div still crosses.
    #[test]
    fn a_generated_clock_and_its_master_are_synchronous() {
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,d,y,z);\ninput clk1,clk2,d; output y,z;\n\
             wire clk_div,div_n,q1,q2;\n\
             DFF u_div(.CK(clk1),.D(div_n),.Q(clk_div));\nINV u_dinv(.A(clk_div),.Y(div_n));\n\
             DFF a(.CK(clk1),.D(d),.Q(q1));\nDFF b(.CK(clk2),.D(d),.Q(q2));\n\
             DFF ca(.CK(clk_div),.D(q1),.Q(y));\nDFF cb(.CK(clk_div),.D(q2),.Q(z));\nendmodule\n",
        )
        .unwrap();
        let base = "create_clock -name clk1 -period 10 [get_ports clk1]\n\
                    create_clock -name clk2 -period 7 [get_ports clk2]\n\
                    create_generated_clock -name clk_div -source [get_ports clk1] -divide_by 2 [get_pins u_div/Q]\n";
        let run = |extra: &str| analyze(&nl, &lib(), &Sdc::parse(&format!("{base}{extra}")).unwrap()).unwrap();
        let r = run("");
        let to: Vec<&str> = r.crossings.iter().map(|c| c.to_flop.as_str()).collect();
        assert_eq!(to, ["cb"], "only clk2 -> clk_div crosses");
        assert_eq!(r.synchronous_skipped, 1, "clk1 -> clk_div is one family");
        // separated by the SDC: the family pair is a crossing again
        let r = run("set_clock_groups -asynchronous -group clk1 -group {clk_div clk2}\n");
        assert!(r.crossings.iter().any(|c| c.to_flop == "ca"), "{:?}", r.crossings);
        assert_eq!(r.synchronous_skipped, 0);
    }

    /// Two bits of one bus, each with its own clean 2-flop synchronizer.
    const BUS_V: &str = "module t(clk1,clk2,d0,d1,y0,y1);\n\
         input clk1,clk2,d0,d1; output y0,y1;\nwire a0,a1,s0,s1;\n\
         DFF \\data_reg[0] (.CK(clk1),.D(d0),.Q(a0));\n\
         DFF \\data_reg[1] (.CK(clk1),.D(d1),.Q(a1));\n\
         DFF \\sync1_reg[0] (.CK(clk2),.D(a0),.Q(s0));\n\
         DFF \\sync2_reg[0] (.CK(clk2),.D(s0),.Q(y0));\n\
         DFF \\sync1_reg[1] (.CK(clk2),.D(a1),.Q(s1));\n\
         DFF \\sync2_reg[1] (.CK(clk2),.D(s1),.Q(y1));\nendmodule\n";

    #[test]
    fn a_bus_synchronized_bit_by_bit_is_reported_as_a_multi_bit_crossing() {
        // THE CASE THAT READ CLEAN. Every bit has a textbook two-flop synchronizer, so every
        // bit is `OK` and the unsynchronized count is zero — and the bus is still broken,
        // because the two chains resolve on independent edges and the receiver can latch a
        // combination that never existed at the source.
        let nl = crate::netlist::parse(BUS_V).unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        assert_eq!(r.crossings.len(), 2);
        assert!(
            r.crossings.iter().all(|c| c.synchronized),
            "each bit really is synchronized — that is what makes this silent"
        );
        assert_eq!(r.multibit.len(), 1, "and the bus is one finding");
        let m = &r.multibit[0];
        assert_eq!(m.bus_from, "data_reg");
        assert_eq!(m.bus_to, "sync1_reg");
        assert_eq!(m.bits, vec![0, 1]);
        assert_eq!(m.bit_span(), "[1:0]");
        assert_eq!(
            (m.from_domain.as_str(), m.to_domain.as_str()),
            ("clk1", "clk2")
        );
    }

    #[test]
    fn a_single_bit_crossing_is_not_a_bus() {
        // One bit of a bus crossing alone is just a synchronized crossing. Reporting it as a
        // multi-bit finding would put a warning on the most ordinary correct construct there
        // is, which is how a check gets switched off.
        let one = BUS_V
            .replace("DFF \\data_reg[1] (.CK(clk1),.D(d1),.Q(a1));\n", "")
            .replace("DFF \\sync1_reg[1] (.CK(clk2),.D(a1),.Q(s1));\n", "")
            .replace("DFF \\sync2_reg[1] (.CK(clk2),.D(s1),.Q(y1));\n", "")
            .replace(",y1)", ")")
            .replace(" output y0,y1;", " output y0;")
            .replace("wire a0,a1,s0,s1;", "wire a0,s0;")
            .replace(",d1;", ";")
            .replace(",d0,d1,", ",d0,");
        let nl = crate::netlist::parse(&one).unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        assert_eq!(r.crossings.len(), 1);
        assert!(r.multibit.is_empty(), "one bit is not a bus");
    }

    #[test]
    fn bits_that_are_not_all_synchronized_stay_ordinary_violations() {
        // A bus whose bits lack synchronizers is already reported bit by bit, loudly. Adding a
        // multi-bit finding on top would report the same defect twice under two names; the new
        // finding exists only for the case that currently reads clean.
        let unsafe_bus = BUS_V
            .replace("DFF \\sync2_reg[0] (.CK(clk2),.D(s0),.Q(y0));\n", "")
            .replace("DFF \\sync2_reg[1] (.CK(clk2),.D(s1),.Q(y1));\n", "")
            .replace(".Q(s0));", ".Q(y0));")
            .replace(".Q(s1));", ".Q(y1));")
            .replace("wire a0,a1,s0,s1;", "wire a0,a1;");
        let nl = crate::netlist::parse(&unsafe_bus).unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        assert_eq!(r.crossings.len(), 2);
        assert!(r.crossings.iter().all(|c| !c.synchronized));
        assert!(r.multibit.is_empty(), "already reported as two violations");
    }

    /// A clock mux: `cap` runs on clk1 **or** clk2 depending on `sel`, and is fed by a flop on
    /// each. `{A}` / `{B}` are substituted to write the same design with the mux inputs wired
    /// in either order.
    const MUX_V: &str = "module t(clk1,clk2,sel,d,y);\ninput clk1,clk2,sel,d; output y;\n\
         wire mclk,q1,q2;\n\
         MUX2 u_mux(.A({A}),.B({B}),.S(sel),.Y(mclk));\n\
         DFF s1(.CK(clk1),.D(d),.Q(q1));\n\
         DFF s2(.CK(clk2),.D(q1),.Q(q2));\n\
         DFF cap(.CK(mclk),.D(q1),.Q(y));\nendmodule\n";

    fn mux_lib() -> Lib {
        // The demo library has no mux; the shape is all this needs.
        let mut text = std::fs::read_to_string("examples/cells.lib").expect("cells.lib");
        let end = text.rfind('}').expect("library close");
        text.insert_str(
            end,
            "  cell (MUX2) {\n    pin (A) { direction : input; }\n\
             \x20   pin (B) { direction : input; }\n    pin (S) { direction : input; }\n\
             \x20   pin (Y) { direction : output; }\n  }\n",
        );
        Lib::parse(&text).expect("lib with a mux")
    }

    #[test]
    fn a_muxed_clock_puts_a_flop_in_every_domain_that_reaches_it() {
        // THE PIN-ORDER BUG. Tracing returned the first clock it found, so this design reported
        // 0 crossings or 1 depending on which mux input the netlist happened to name first —
        // and both answers were incomplete, because the flop really is clocked by both.
        let lib = mux_lib();
        for (a, b) in [("clk1", "clk2"), ("clk2", "clk1")] {
            let nl = crate::netlist::parse(&MUX_V.replace("{A}", a).replace("{B}", b)).unwrap();
            let r = analyze(&nl, &lib, &sdc()).unwrap();
            assert_eq!(
                r.flop_domain.get("cap"),
                Some(&vec!["clk1".to_string(), "clk2".to_string()]),
                "cap is clocked by both, wired {a}/{b}"
            );
            assert_eq!(r.multi_clocked, vec!["cap".to_string()]);
            // s1 (clk1) -> cap: a crossing in cap's clk2 domain and not in its clk1 one.
            let into_cap: Vec<_> = r.crossings.iter().filter(|c| c.to_flop == "cap").collect();
            assert_eq!(into_cap.len(), 1, "wired {a}/{b}");
            assert_eq!(
                (
                    into_cap[0].from_domain.as_str(),
                    into_cap[0].to_domain.as_str()
                ),
                ("clk1", "clk2")
            );
        }
    }

    #[test]
    fn the_verdict_does_not_depend_on_the_order_the_mux_was_wired() {
        // A sign-off answer that changes when a synthesiser reorders a connection is not an
        // answer. Same design, both spellings, byte-identical findings.
        let lib = mux_lib();
        let render = |a: &str, b: &str| {
            let nl = crate::netlist::parse(&MUX_V.replace("{A}", a).replace("{B}", b)).unwrap();
            let r = analyze(&nl, &lib, &sdc()).unwrap();
            r.crossings
                .iter()
                .map(|c| {
                    format!(
                        "{} [{}] -> {} [{}] sync={}",
                        c.from_flop, c.from_domain, c.to_flop, c.to_domain, c.synchronized
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(render("clk1", "clk2"), render("clk2", "clk1"));
    }

    #[test]
    fn combinational_logic_on_crossing_is_through_logic() {
        // A (clk1) -> INV -> B (clk2): logic on the crossing path, not synchronized.
        let nl = crate::netlist::parse(
            "module t(clk1,clk2,y);\ninput clk1,clk2; output y;\nwire q,n;\n\
             DFF a(.CK(clk1),.D(y),.Q(q));\nINV g(.A(q),.Y(n));\nDFF b(.CK(clk2),.D(n),.Q(y));\nendmodule\n",
        )
        .unwrap();
        let r = analyze(&nl, &lib(), &sdc()).unwrap();
        let c: Vec<_> = r.crossings.iter().filter(|c| c.to_flop == "b").collect();
        assert_eq!(c.len(), 1);
        assert!(c[0].through_logic && !c[0].synchronized);
    }
}
