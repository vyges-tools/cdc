//! Reset-domain-crossing analysis.
//!
//! The load-bearing tests here are the negative ones. A checker that reports a crossing on a
//! correctly synchronized path, or on a synchronous reset, is a checker that gets switched off
//! — and "clear, low noise reporting" is the stated reason teams buy the commercial equivalent.

use vyges_cdc::liberty::Lib;
use vyges_cdc::netlist::{self, Netlist};
use vyges_cdc::rdc;

/// Two async-reset flops (`dfrtp`, reset `RESET_B`), one plain flop, and a buffer/inverter to
/// trace resets through.
fn lib() -> Lib {
    Lib::parse(
        r#"library(t) {
  cell (dfrtp) {
    ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; clear : "!RESET_B"; }
    pin (CLK) { direction : input; clock : true; }
    pin (D) { direction : input;
      timing () { related_pin : "CLK"; timing_type : setup_rising; }
      timing () { related_pin : "CLK"; timing_type : hold_rising; } }
    pin (RESET_B) { direction : input; }
    pin (Q) { direction : output; }
  }
  cell (dfxtp) {
    ff (IQ, IQN) { clocked_on : "CLK"; next_state : "D"; }
    pin (CLK) { direction : input; clock : true; }
    pin (D) { direction : input;
      timing () { related_pin : "CLK"; timing_type : setup_rising; }
      timing () { related_pin : "CLK"; timing_type : hold_rising; } }
    pin (Q) { direction : output; }
  }
  cell (buf) {
    pin (A) { direction : input; }
    pin (X) { direction : output; }
  }
  cell (and2) {
    pin (A) { direction : input; }
    pin (B) { direction : input; }
    pin (X) { direction : output; }
  }
}
"#,
    )
    .unwrap()
}

fn nl(body: &str) -> Netlist {
    netlist::parse(&format!(
        "module top(clk, rst_a, rst_b, din);\n input clk, rst_a, rst_b, din;\n{body}endmodule\n"
    ))
    .unwrap()
}

#[test]
fn a_crossing_between_two_reset_domains_is_reported() {
    // src reset by rst_a, dst reset by rst_b, Q -> D directly.
    let n = nl("\
 dfrtp src (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 dfrtp dst (.CLK(clk), .D(mid), .RESET_B(rst_b), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(r.crossings.len(), 1, "one crossing: {:?}", r.crossings);
    let c = &r.crossings[0];
    assert_eq!((c.from_flop.as_str(), c.to_flop.as_str()), ("src", "dst"));
    assert_eq!(
        (c.from_domain.as_str(), c.to_domain.as_str()),
        ("rst_a", "rst_b")
    );
    assert!(!c.synchronized, "no second stage present");
    assert_eq!(r.domains, vec!["rst_a", "rst_b"]);
}

#[test]
fn one_reset_domain_is_not_a_crossing() {
    let n = nl("\
 dfrtp src (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 dfrtp dst (.CLK(clk), .D(mid), .RESET_B(rst_a), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert!(r.crossings.is_empty(), "same reset: {:?}", r.crossings);
    assert_eq!(r.domains, vec!["rst_a"]);
}

#[test]
fn a_reset_traced_through_a_buffer_is_the_same_domain() {
    // The classic false positive: one reset, buffered on the way to one of the two flops.
    // If the trace stopped at the buffer output this would report a crossing that is not there.
    let n = nl("\
 buf rbuf (.A(rst_a), .X(rst_a_buf));\n\
 dfrtp src (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 dfrtp dst (.CLK(clk), .D(mid), .RESET_B(rst_a_buf), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert!(
        r.crossings.is_empty(),
        "a buffered reset is the same reset: {:?}",
        r.crossings
    );
    assert_eq!(r.domains, vec!["rst_a"], "one domain, named at its origin");
}

#[test]
fn a_two_flop_synchronizer_is_recognized_and_not_flagged_unsynchronized() {
    // src(rst_a) -> s1(rst_b) -> s2(rst_b): the canonical protection.
    let n = nl("\
 dfrtp src (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 dfrtp s1  (.CLK(clk), .D(mid), .RESET_B(rst_b), .Q(s1q));\n\
 dfrtp s2  (.CLK(clk), .D(s1q), .RESET_B(rst_b), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(r.crossings.len(), 1, "still one crossing, but protected");
    assert!(
        r.crossings[0].synchronized,
        "two-flop synchronizer must be recognized, or every protected design reports violations"
    );
    assert!(!r.crossings[0].through_logic);
}

#[test]
fn logic_on_the_crossing_path_is_flagged_and_never_called_synchronized() {
    // A gate between the domains defeats a synchronizer's first stage — it can glitch.
    let n = nl("\
 dfrtp src (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 and2  g   (.A(mid), .B(din), .X(gated));\n\
 dfrtp s1  (.CLK(clk), .D(gated), .RESET_B(rst_b), .Q(s1q));\n\
 dfrtp s2  (.CLK(clk), .D(s1q), .RESET_B(rst_b), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(r.crossings.len(), 1);
    assert!(
        r.crossings[0].through_logic,
        "logic on the path is the finding"
    );
    assert!(
        !r.crossings[0].synchronized,
        "a synchronizer fed through logic is not a synchronizer"
    );
}

#[test]
fn a_flop_with_no_async_reset_is_not_a_reset_domain() {
    // The noise case that matters most. A synchronous reset arrives on D, is timed like any
    // other data, and cannot race on deassertion. Treating an unreset flop as its own domain
    // would make almost every design report crossings everywhere.
    let n = nl("\
 dfxtp src (.CLK(clk), .D(din), .Q(mid));\n\
 dfrtp dst (.CLK(clk), .D(mid), .RESET_B(rst_b), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert!(
        r.crossings.is_empty(),
        "an unreset launching flop is not a reset crossing: {:?}",
        r.crossings
    );
    assert_eq!(
        r.unreset_flops, 1,
        "counted, so a clean run is not mistaken for an empty one"
    );
}

#[test]
fn a_reset_synchronizer_output_is_its_own_domain() {
    // rst_a through a flop is a *synchronized* reset — a different domain from raw rst_a,
    // which is the whole point of putting the synchronizer there.
    let n = nl("\
 dfrtp rsync (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(rst_a_sync));\n\
 dfrtp src   (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(mid));\n\
 dfrtp dst   (.CLK(clk), .D(mid), .RESET_B(rst_a_sync), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(
        r.crossings.len(),
        1,
        "raw reset -> synchronized reset is a genuine domain boundary: {:?}",
        r.crossings
    );
    assert_eq!(r.crossings[0].to_domain, "rst_a_sync");
}

#[test]
fn a_netlist_with_no_flops_is_reported_as_nothing_to_check() {
    // Found by validating against a real TL-UL crossbar netlist: it has no sequential cells at
    // all, and "0 crossings" read exactly like a clean result on a design full of them.
    // `unreset_flops` does not catch this — zero flops scores zero there too.
    let n = nl(" and2 g (.A(din), .B(din), .X(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(r.seq_flops, 0, "there are genuinely no flops");
    assert_eq!(r.unreset_flops, 0);
    assert!(r.crossings.is_empty());
}

#[test]
fn the_flop_census_distinguishes_checked_from_unexaminable() {
    let n = nl("\
 dfrtp a (.CLK(clk), .D(din), .RESET_B(rst_a), .Q(q1));\n\
 dfxtp b (.CLK(clk), .D(q1), .Q(q2));\n\
 dfxtp c (.CLK(clk), .D(q2), .Q(dout));\n");
    let r = rdc::analyze(&n, &lib()).unwrap();
    assert_eq!(r.seq_flops, 3, "all three are sequential");
    assert_eq!(r.unreset_flops, 2, "two carry no async reset");
    // 3 - 2 = 1 flop was actually eligible for reset-domain analysis, which is what the
    // report must be able to say.
}
