//! `--rtl`: the elaborated RTL netlist read by RTL register names (each register instance by the
//! net on its stored-value output), so crossings name registers as the RTL does and an SDC written
//! against the RTL applies as written.

use std::process::Command;

const LIB: &str = r#"library(t) {
  cell (dff) {
    ff (IQ, IQN) { clocked_on : "CK"; next_state : "D"; }
    pin (CK) { direction : input; clock : true; }
    pin (D) { direction : input;
      timing () { related_pin : "CK"; timing_type : setup_rising; }
      timing () { related_pin : "CK"; timing_type : hold_rising; } }
    pin (Q) { direction : output; function : "IQ"; }
  }
  cell (buf) {
    pin (A) { direction : input; }
    pin (X) { direction : output; function : "A"; }
  }
}
"#;

/// An elaborated netlist as a front end that keeps RTL names writes it: register instances
/// `u1`, `u2`, their RTL names on the Q nets (`a_q` in clock domain `ca`, `b_q` in `cb`).
const NET: &str = "module t(ca, cb, d, y);\ninput ca, cb, d; output y;\nwire a_q, b_q;\n\
  dff u1(.CK(ca), .D(d), .Q(a_q));\ndff u2(.CK(cb), .D(a_q), .Q(b_q));\nbuf u3(.A(b_q), .X(y));\nendmodule\n";

/// The constraint file written against the RTL: the crossing a_q -> b_q is declared false.
const SDC: &str = "create_clock -name ca -period 10 [get_ports ca]\n\
  create_clock -name cb -period 7 [get_ports cb]\n\
  set_false_path -from [get_cells a_q] -to [get_cells b_q]\n";

fn run(dir: &std::path::Path, rtl: bool) -> (String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_vyges-cdc"));
    c.arg("check").arg(dir.join("t.v")).arg("--lib").arg(dir.join("t.lib")).arg("--sdc").arg(dir.join("t.sdc")).arg("--json");
    if rtl {
        c.arg("--rtl");
    }
    let o = c.output().expect("run vyges-cdc");
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn rtl_names_the_crossing_and_applies_the_rtl_sdc() {
    let dir = std::env::temp_dir().join(format!("vyges-cdc-rtl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.lib"), LIB).unwrap();
    std::fs::write(dir.join("t.v"), NET).unwrap();
    std::fs::write(dir.join("t.sdc"), SDC).unwrap();
    // as written: the crossing is between instances u1 -> u2, and the SDC's register names
    // name no instance, so the false path does not cover it
    let (out, err) = run(&dir, false);
    assert!(out.contains("\"from\": \"u1\", \"to\": \"u2\""), "{out}{err}");
    assert!(out.contains("\"setup_timed\": true"), "{out}");
    // --rtl: the crossing is a_q -> b_q, and the false path written against the RTL untimes it
    let (out, err) = run(&dir, true);
    assert!(out.contains("\"from\": \"a_q\", \"to\": \"b_q\""), "{out}{err}");
    assert!(out.contains("\"setup_timed\": false"), "{out}");
    assert!(err.contains("RTL view: 2 register(s) named by their RTL name, 0 kept"), "{err}");
}
