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

pub fn analyze(nl: &Netlist, lib: &Lib, sdc: &Sdc) -> Result<CdcReport, String> {
    if lib.cells.is_empty() {
        return Err("no cells in the Liberty".into());
    }
    // SDC clock source (port or inst/pin) -> domain name
    let mut src: BTreeMap<String, String> = BTreeMap::new();
    for c in &sdc.clocks {
        src.insert(c.source.clone(), c.name.clone());
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
    for inst in &nl.insts {
        if let Some((clk, _, _)) = flop_pins(lib, &inst.cell) {
            if let Some(cn) = net_of(inst, &clk) {
                if let Some(dom) = trace_clock(cn, &nd, nl, lib, &src, &mut BTreeSet::new()) {
                    flop_domain.insert(inst.name.clone(), dom);
                }
            }
        }
    }

    // crossings: for each capture flop, walk its D cone to launch flops
    let mut crossings = Vec::new();
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
