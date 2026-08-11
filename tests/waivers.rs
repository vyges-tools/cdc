//! Waivers, end to end against a real report.
//!
//! The load-bearing property is not that a waiver silences a finding — it is that silencing it
//! stays visible and stays temporary. A waiver that hides a finding without saying so, or that
//! outlives the reason it was written for, is worse than no waiver mechanism at all: it turns a
//! clean report into an unfalsifiable one.

use vyges_cdc::cdc;
use vyges_cdc::liberty::Lib;
use vyges_cdc::netlist;
use vyges_cdc::sdc::Sdc;
use vyges_cdc::waive::{self, WaiverSet};

const LIB: &str = r#"library (t) {
  cell (DFF) {
    ff (IQ, IQN) { clocked_on : "CK"; next_state : "D"; }
    pin (CK) { direction : input; clock : true; }
    pin (D) { direction : input;
      timing () { related_pin : "CK"; timing_type : setup_rising; }
      timing () { related_pin : "CK"; timing_type : hold_rising; } }
    pin (Q) { direction : output; }
  }
}
"#;

/// `a` (clk1) → `b` (clk2) with no synchronizer, plus a two-bit bus each of whose bits has its
/// own clean synchronizer: one of each finding kind in one design.
const NL: &str = "module t(clk1,clk2,d,d0,d1,y,y0,y1);\n\
     input clk1,clk2,d,d0,d1; output y,y0,y1;\n\
     wire q,a0,a1,s0,s1;\n\
     DFF a (.CK(clk1),.D(d),.Q(q));\n\
     DFF b (.CK(clk2),.D(q),.Q(y));\n\
     DFF \\data_reg[0] (.CK(clk1),.D(d0),.Q(a0));\n\
     DFF \\data_reg[1] (.CK(clk1),.D(d1),.Q(a1));\n\
     DFF \\sync1_reg[0] (.CK(clk2),.D(a0),.Q(s0));\n\
     DFF \\sync2_reg[0] (.CK(clk2),.D(s0),.Q(y0));\n\
     DFF \\sync1_reg[1] (.CK(clk2),.D(a1),.Q(s1));\n\
     DFF \\sync2_reg[1] (.CK(clk2),.D(s1),.Q(y1));\n\
     endmodule\n";

const SDC: &str = "create_clock -name clk1 -period 10 [get_ports clk1]\n\
                   create_clock -name clk2 -period 7 [get_ports clk2]\n";

fn report() -> cdc::CdcReport {
    let nl = netlist::parse(NL).expect("netlist");
    let lib = Lib::parse(LIB).expect("lib");
    let sdc = Sdc::parse(SDC).expect("sdc");
    cdc::analyze(&nl, &lib, &sdc).expect("analyze")
}

fn day(s: &str) -> i64 {
    waive::parse_date(s).expect("date")
}

#[test]
fn the_design_has_one_of_each_finding_before_any_waiver() {
    let r = report();
    assert_eq!(r.crossings.iter().filter(|c| !c.synchronized).count(), 1);
    assert_eq!(r.multibit.len(), 1);
}

#[test]
fn a_waiver_accepts_a_finding_and_the_report_still_shows_what_was_accepted() {
    let mut r = report();
    let set = WaiverSet::parse(
        "waive: multibit\nfrom: data_reg\nreason: gray-coded pointer\nexpires: 2027-01-01\n",
    )
    .unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));

    assert!(r.multibit.is_empty(), "the finding leaves the active list");
    assert_eq!(out.waived.len(), 1, "and appears as waived");
    assert!(out.waived[0].reason.contains("gray-coded"));
    assert!(out.waived[0].what.contains("data_reg[1:0]"));
    // The unsynchronized crossing was not waived and is untouched.
    assert_eq!(r.crossings.iter().filter(|c| !c.synchronized).count(), 1);
    assert!(out.lapsed.is_empty() && out.stale.is_empty());
}

#[test]
fn a_lapsed_waiver_does_not_apply_and_says_so() {
    // The reason expiry exists. An accepted finding must come back when the acceptance runs
    // out, or "reviewed in 2026" silently becomes "never looked at again".
    let mut r = report();
    let set = WaiverSet::parse(
        "waive: multibit\nfrom: data_reg\nreason: gray-coded pointer\nexpires: 2026-12-31\n",
    )
    .unwrap();
    let out = waive::apply(&mut r, &set, day("2027-06-01"));
    assert_eq!(r.multibit.len(), 1, "the finding is back");
    assert!(out.waived.is_empty());
    assert_eq!(out.lapsed, vec![0]);
    assert!(out.stale.is_empty(), "lapsed is not the same as stale");
}

#[test]
fn a_waiver_matching_nothing_is_reported_as_stale() {
    // A waiver for a finding that no longer exists is a claim about a design that moved on.
    // Unreported, it stays in the file forever and the file stops being read.
    let mut r = report();
    let set =
        WaiverSet::parse("waive: crossing\nfrom: long_gone_reg\nreason: fixed in v2\n").unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));
    assert_eq!(out.stale, vec![0]);
    assert!(out.waived.is_empty());
    assert_eq!(r.crossings.iter().filter(|c| !c.synchronized).count(), 1);
}

#[test]
fn a_waiver_only_covers_the_kind_and_the_names_it_names() {
    // The failure that matters is a waiver quietly covering more than it was written for.
    let mut r = report();
    // Right names, wrong kind: the multi-bit waiver must not swallow the lone crossing.
    let set = WaiverSet::parse("waive: multibit\nreason: everything multibit\n").unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));
    assert_eq!(out.waived.len(), 1);
    assert_eq!(
        r.crossings.iter().filter(|c| !c.synchronized).count(),
        1,
        "the unsynchronized crossing is a different kind of finding"
    );

    // Wrong clock direction: same names, crossing the other way, must not match.
    let mut r = report();
    let set = WaiverSet::parse(
        "waive: crossing\nfrom: a\nto: b\nfrom_clock: clk2\nto_clock: clk1\nreason: reversed\n",
    )
    .unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));
    assert!(out.waived.is_empty(), "clk1->clk2 is not clk2->clk1");
    assert_eq!(out.stale, vec![0]);
}

#[test]
fn a_prefix_pattern_waives_a_subtree_and_the_count_makes_its_reach_visible() {
    let mut r = report();
    let set = WaiverSet::parse("waive: any\nfrom: *\nreason: whole-design amnesty\n").unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));
    // Both findings, one waiver — which is exactly the shape that needs to be visible, so the
    // count is reported rather than the findings just vanishing.
    assert_eq!(out.waived.len(), 2);
    assert!(r.multibit.is_empty());
    assert_eq!(r.crossings.iter().filter(|c| !c.synchronized).count(), 0);
    assert_eq!(
        out.no_expiry, 1,
        "and it carries no expiry, which is counted"
    );
}

#[test]
fn a_synchronized_crossing_is_never_waived_because_it_was_never_a_finding() {
    let mut r = report();
    let before = r.crossings.len();
    let set = WaiverSet::parse("waive: any\nreason: everything\n").unwrap();
    let out = waive::apply(&mut r, &set, day("2026-08-11"));
    assert_eq!(
        r.crossings.len(),
        before - 1,
        "only the one violation left the list; both OK crossings stay"
    );
    assert!(r.crossings.iter().all(|c| c.synchronized));
    assert_eq!(out.waived.len(), 2, "the violation and the multi-bit bus");
}
