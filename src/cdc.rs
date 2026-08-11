//! The CDC analysis engine.
//!
//! Builds a net→driver graph, assigns each flop a clock domain (trace its clock
//! pin back to an SDC clock source), walks each capture flop's data cone back to
//! its launching flops, and reports every cross-domain launch→capture pair —
//! classifying the canonical two-flop synchronizer.

use std::collections::{BTreeMap, BTreeSet};

use crate::liberty::{Dir, Lib};
use crate::netlist::{Inst, Netlist};
use crate::sdc::Sdc;

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
}

#[derive(Debug, Default)]
pub struct CdcReport {
    pub crossings: Vec<Crossing>,
    pub flop_domain: BTreeMap<String, String>, // flop instance -> domain
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
    /// Launch→capture pairs in *different* domains that the SDC declares **related** to each
    /// other, so they are timed rather than crossed. Counted, not reported as crossings —
    /// a number here says the clock grouping was read and applied.
    pub related_skipped: usize,
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

/// Trace a clock net back (through combinational clock cells) to an SDC clock
/// source; return its domain name.
fn trace_clock(
    net: &str,
    nd: &BTreeMap<String, Driver>,
    nl: &Netlist,
    lib: &Lib,
    src: &BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
) -> Option<String> {
    if let Some(d) = src.get(net) {
        return Some(d.clone());
    }
    if !seen.insert(net.to_string()) {
        return None;
    }
    let drv = nd.get(net)?;
    let i = drv.inst?; // a port that isn't an SDC clock source -> unknown
    if drv.is_seq {
        return None; // divided/gated clock off a flop — not modelled in v0
    }
    let inst = &nl.insts[i];
    for (pin, n) in &inst.conns {
        if is_in(lib, &inst.cell, pin) {
            if let Some(d) = trace_clock(n, nd, nl, lib, src, seen) {
                return Some(d);
            }
        }
    }
    None
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
/// A flattened netlist puts hierarchy in the *instance* name (`core/u_div/Q`), so the split is
/// at the **last** separator, not the first.
fn source_net<'a>(source: &str, nl: &'a Netlist) -> Option<&'a str> {
    let (inst_name, pin) = source.rsplit_once('/')?;
    let inst = nl.insts.iter().find(|i| i.name == inst_name)?;
    net_of(inst, pin)
}

/// Are two domains asynchronous to each other?
///
/// `set_clock_groups` is the SDC's own statement about this: clocks inside one `-group` are
/// related, clocks in different groups are not. Without it every differently-named clock is a
/// crossing, which on a real SoC reports every synchronous divide off one PLL as a CDC
/// violation — noise that gets a checker switched off.
///
/// A clock the grouping never mentions is unconstrained, and is treated as asynchronous: the
/// conservative reading, and the one that keeps an SDC declaring nothing behaving exactly as
/// before.
fn asynchronous(a: &str, b: &str, groups: &[Vec<String>]) -> bool {
    if a == b {
        return false;
    }
    let group_of = |c: &str| groups.iter().position(|g| g.iter().any(|n| n == c));
    match (group_of(a), group_of(b)) {
        (Some(x), Some(y)) => x != y,
        _ => true,
    }
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

    // domain per flop instance (trace clock pin)
    let mut flop_domain: BTreeMap<String, String> = BTreeMap::new();
    let mut flops_total = 0usize;
    let mut unplaced: Vec<String> = Vec::new();
    for inst in &nl.insts {
        let Some((clk, _, _)) = flop_pins(lib, &inst.cell) else {
            continue;
        };
        flops_total += 1;
        let dom = net_of(inst, &clk)
            .and_then(|cn| trace_clock(cn, &nd, nl, lib, &src, &mut BTreeSet::new()));
        match dom {
            Some(d) => {
                flop_domain.insert(inst.name.clone(), d);
            }
            // Unplaced: a divided or gated clock off a flop, or a clock port the SDC never
            // declared. Recorded rather than passed over, because everything downstream skips
            // this flop and the report would otherwise look complete.
            None => unplaced.push(inst.name.clone()),
        }
    }

    // crossings: for each capture flop, walk its D cone to launch flops
    let mut crossings = Vec::new();
    let mut related_skipped = 0usize;
    for inst in &nl.insts {
        let Some((_, dpins, _)) = flop_pins(lib, &inst.cell) else {
            continue;
        };
        let Some(dc) = flop_domain.get(&inst.name) else {
            continue;
        };
        for d in &dpins {
            let Some(dn) = net_of(inst, d) else { continue };
            let mut launches = Vec::new();
            launch_flops(dn, true, &nd, nl, lib, &mut BTreeSet::new(), &mut launches);
            for (li, direct) in launches {
                let lname = &nl.insts[li].name;
                let Some(dl) = flop_domain.get(lname) else {
                    continue;
                };
                if dl == dc {
                    continue; // same domain, not a crossing
                }
                if !asynchronous(dl, dc, &sdc.async_groups) {
                    related_skipped += 1; // declared related: timed, not crossed
                    continue;
                }
                let synchronized = direct && has_second_stage(inst, dc, lib, nl, &nd, &flop_domain);
                crossings.push(Crossing {
                    from_flop: lname.clone(),
                    from_domain: dl.clone(),
                    to_flop: inst.name.clone(),
                    to_domain: dc.clone(),
                    through_logic: !direct,
                    synchronized,
                });
            }
        }
    }

    let mut domains: Vec<String> = sdc.clocks.iter().map(|c| c.name.clone()).collect();
    domains.sort();
    domains.dedup();
    Ok(CdcReport {
        crossings,
        flop_domain,
        domains,
        flops_total,
        unplaced,
        related_skipped,
    })
}

/// Is the capture flop the first stage of a 2-flop synchronizer? — does its Q
/// directly drive the D of another flop in the same domain (no logic between)?
fn has_second_stage(
    cap: &Inst,
    domain: &str,
    lib: &Lib,
    nl: &Netlist,
    nd: &BTreeMap<String, Driver>,
    flop_domain: &BTreeMap<String, String>,
) -> bool {
    let Some((_, _, qpins)) = flop_pins(lib, &cap.cell) else {
        return false;
    };
    for q in &qpins {
        let Some(qn) = net_of(cap, q) else { continue };
        for s2 in &nl.insts {
            if flop_domain.get(&s2.name).map(String::as_str) != Some(domain) {
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
        assert_eq!(r.flop_domain.get("a"), Some(&"clk1".to_string()));
        assert_eq!(r.flop_domain.get("b"), Some(&"clk2".to_string()));
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
        assert_eq!(r.flop_domain.get("cap"), Some(&"clk_div".to_string()));
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
