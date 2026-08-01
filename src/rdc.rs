//! The RDC analysis engine — **reset**-domain crossings.
//!
//! A flop asynchronously reset by one reset, feeding a flop reset by a different one. When
//! those resets deassert independently, the receiving flop can sample its D input while the
//! launching flop is still settling out of reset — the same metastability failure a CDC check
//! exists to find, from a source no CDC check looks at. Both flops can be on a single clock,
//! so a design can be entirely CDC-clean and still fail this way.
//!
//! The shape mirrors `cdc.rs` deliberately: assign every flop a domain, walk each capture
//! flop's data cone back to its launching flops, report the pairs whose domains differ. The
//! difference is where a domain comes from.
//!
//! **Reset domains are structural, not declared.** SDC has `create_clock`; it has no
//! `create_reset`. So a flop's reset domain is the *net* driving its asynchronous reset pin,
//! traced back through buffers and inverters to whatever originates it — a primary input, or
//! the output of a reset synchronizer. Two flops share a domain when they trace to the same
//! origin. That means the domain names here are net names rather than SDC names, and a design
//! with one reset has exactly one domain and no crossings.
//!
//! Which pins are asynchronous resets comes from the Liberty `ff` group's `clear`/`preset`
//! expressions (`liberty::Cell::async_reset_pins`). A **synchronous** reset is not one of
//! these: it arrives on `next_state`, is timed like any other data path, and cannot cause a
//! deassertion race. Treating it as a reset domain would manufacture crossings that are not
//! there — and a checker that cries wolf is a checker nobody runs.
//!
//! **v0 bound, stated plainly:** this is gate-level and structural. It does not prove
//! protection, analyse reset *sequencing* or ordering, or know that a destination is held in
//! reset while the source deasserts. It reports what crosses and whether a recognizable
//! synchronizer sits on the path.

use std::collections::{BTreeMap, BTreeSet};

use crate::liberty::{Dir, Lib};
use crate::netlist::{Inst, Netlist};

#[derive(Debug, Clone)]
pub struct ResetCrossing {
    pub from_flop: String,
    pub from_domain: String,
    pub to_flop: String,
    pub to_domain: String,
    /// The path runs through combinational logic rather than straight from Q to D.
    pub through_logic: bool,
    /// A two-flop synchronizer was recognized on the destination side.
    pub synchronized: bool,
}

#[derive(Debug, Default)]
pub struct RdcReport {
    pub crossings: Vec<ResetCrossing>,
    /// flop instance -> reset domain (the origin net of its async reset)
    pub flop_domain: BTreeMap<String, String>,
    pub domains: Vec<String>,
    /// Flops with no asynchronous reset at all. Not a finding — most designs are full of
    /// them — but the count is worth reporting so a clean run cannot be mistaken for a run
    /// that found nothing to look at.
    pub unreset_flops: usize,
}

struct Driver {
    inst: Option<usize>,
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

/// `(data_pins, q_pins)` for a sequential cell.
fn flop_data_q(lib: &Lib, cell: &str) -> Option<(Vec<String>, Vec<String>)> {
    let c = lib.cells.get(cell)?;
    if !c.is_seq {
        return None;
    }
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
    Some((d, q))
}

/// Trace a reset net back through combinational cells to the net that originates it.
///
/// Unlike the clock trace in `cdc.rs`, there is no declared list of sources to stop at, so
/// this stops where the graph does: at a primary input, or at a flop output — which is what a
/// reset synchronizer looks like from here, and is exactly the boundary that makes its output
/// a domain of its own rather than part of the domain that feeds it.
fn trace_reset(
    net: &str,
    nd: &BTreeMap<String, Driver>,
    nl: &Netlist,
    lib: &Lib,
    seen: &mut BTreeSet<String>,
) -> String {
    if !seen.insert(net.to_string()) {
        return net.to_string(); // combinational loop — stop where we are
    }
    let Some(drv) = nd.get(net) else {
        return net.to_string(); // undriven: the net itself is the origin
    };
    let Some(i) = drv.inst else {
        return net.to_string(); // primary input — the origin
    };
    if drv.is_seq {
        return net.to_string(); // a synchronized reset: its own domain
    }
    // A buffer or inverter passes the domain through; a gate that combines two resets is
    // ambiguous, so take the first input deterministically and keep the traversal total.
    let inst = &nl.insts[i];
    for (pin, n) in &inst.conns {
        if is_in(lib, &inst.cell, pin) {
            return trace_reset(n, nd, nl, lib, seen);
        }
    }
    net.to_string()
}

/// Walk a data net's combinational cone back to launching flops.
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
    let Some(i) = drv.inst else { return };
    if drv.is_seq {
        out.push((i, direct));
        return;
    }
    let inst = &nl.insts[i];
    for (pin, n) in &inst.conns {
        if is_in(lib, &inst.cell, pin) {
            launch_flops(n, false, nd, nl, lib, seen, out);
        }
    }
}

pub fn analyze(nl: &Netlist, lib: &Lib) -> Result<RdcReport, String> {
    if lib.cells.is_empty() {
        return Err("no cells in the Liberty".into());
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

    // domain per flop: the origin of whatever drives its async reset pin
    let mut flop_domain: BTreeMap<String, String> = BTreeMap::new();
    let mut unreset_flops = 0usize;
    for inst in &nl.insts {
        let Some(cell) = lib.cells.get(&inst.cell) else {
            continue;
        };
        if !cell.is_seq {
            continue;
        }
        let mut dom = None;
        for rp in &cell.async_reset_pins {
            if let Some(rn) = net_of(inst, rp) {
                dom = Some(trace_reset(rn, &nd, nl, lib, &mut BTreeSet::new()));
                break; // one domain per flop; a cell with both clear and preset tied to
                       // different resets is rare and out of scope for v0
            }
        }
        match dom {
            Some(d) => {
                flop_domain.insert(inst.name.clone(), d);
            }
            None => unreset_flops += 1,
        }
    }

    // crossings: for each reset-bearing capture flop, walk its D cone to launch flops
    let mut crossings = Vec::new();
    for inst in &nl.insts {
        let Some((dpins, _)) = flop_data_q(lib, &inst.cell) else {
            continue;
        };
        let Some(dc) = flop_domain.get(&inst.name) else {
            continue; // no async reset on the capture side — nothing to race
        };
        for d in &dpins {
            let Some(dn) = net_of(inst, d) else { continue };
            let mut launches = Vec::new();
            launch_flops(dn, true, &nd, nl, lib, &mut BTreeSet::new(), &mut launches);
            for (li, direct) in launches {
                let lname = &nl.insts[li].name;
                let Some(dl) = flop_domain.get(lname) else {
                    continue; // launching flop has no async reset — not a reset crossing
                };
                if dl == dc {
                    continue;
                }
                let synchronized = direct && has_second_stage(inst, dc, lib, nl, &nd, &flop_domain);
                crossings.push(ResetCrossing {
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

    let mut domains: Vec<String> = flop_domain.values().cloned().collect();
    domains.sort();
    domains.dedup();
    Ok(RdcReport {
        crossings,
        flop_domain,
        domains,
        unreset_flops,
    })
}

/// Is the capture flop the first stage of a two-flop synchronizer — does its Q directly drive
/// the D of another flop in the *same reset domain*, with no logic between?
fn has_second_stage(
    cap: &Inst,
    domain: &str,
    lib: &Lib,
    nl: &Netlist,
    nd: &BTreeMap<String, Driver>,
    flop_domain: &BTreeMap<String, String>,
) -> bool {
    let Some((_, qpins)) = flop_data_q(lib, &cap.cell) else {
        return false;
    };
    for q in &qpins {
        let Some(qn) = net_of(cap, q) else { continue };
        // Only Q's *direct* driver counts; logic in between is not a synchronizer.
        if nd.get(qn).and_then(|d| d.inst).map(|i| &nl.insts[i].name) != Some(&cap.name) {
            continue;
        }
        for s2 in &nl.insts {
            if s2.name == cap.name {
                continue;
            }
            if flop_domain.get(&s2.name).map(String::as_str) != Some(domain) {
                continue;
            }
            let Some((d2pins, _)) = flop_data_q(lib, &s2.cell) else {
                continue;
            };
            if d2pins.iter().any(|d2| net_of(s2, d2) == Some(qn)) {
                return true;
            }
        }
    }
    false
}
