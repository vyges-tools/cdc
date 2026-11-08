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

/// `--rtl` reads the SDC's names as `vyges physical sdc map` does, by the same loom code: an RTL
/// form of a register name by its alias (`b_q_reg` is `b_q`), and with `--ranges` a declared bit
/// index (`a_q_reg[8]` of [9:8] is `a_q[0]`), each reported (CDC-MAP-ALIAS, CDC-MAP-BIT). Without
/// the ranges, `a_q[8]` names nothing and the false path does not cover the crossing.
#[test]
fn rtl_reads_sdc_bit_indices_and_aliases_as_map_does() {
    let dir = std::env::temp_dir().join(format!("vyges-cdc-rtl-names-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.lib"), LIB).unwrap();
    std::fs::write(
        dir.join("t.v"),
        "module t(ca, cb, d, y);\ninput ca, cb, d; output y;\nwire [1:0] a_q; wire b_q;\n\
         dff u1(.CK(ca), .D(d), .Q(a_q[0]));\ndff u2(.CK(ca), .D(d), .Q(a_q[1]));\n\
         dff u3(.CK(cb), .D(a_q[0]), .Q(b_q));\nbuf u4(.A(b_q), .X(y));\nendmodule\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("t.sdc"),
        "create_clock -name ca -period 10 [get_ports ca]\n\
         create_clock -name cb -period 7 [get_ports cb]\n\
         set_false_path -from [get_cells {a_q_reg[8]}] -to [get_cells b_q_reg]\n",
    )
    .unwrap();
    std::fs::write(dir.join("r.json"), r#"{ "ranges": { "a_q": [9, 8] } }"#).unwrap();
    let run = |extra: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_vyges-cdc"))
            .arg("check").arg(dir.join("t.v")).arg("--lib").arg(dir.join("t.lib")).arg("--sdc").arg(dir.join("t.sdc")).arg("--json")
            .args(extra)
            .output()
            .expect("run vyges-cdc");
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
    };
    let ranges = dir.join("r.json");
    let ranges = ranges.to_str().unwrap();
    let (_, out, err) = run(&["--rtl", "--ranges", ranges]);
    assert!(out.contains("\"from\": \"a_q[0]\", \"to\": \"b_q\""), "{out}{err}");
    assert!(out.contains("\"setup_timed\": false"), "{out}{err}");
    assert!(err.contains("get_cells a_q_reg[8] is elaborated a_q[0] (declared [9:8])") && err.contains("CDC-MAP-BIT"), "{err}");
    assert!(err.contains("get_cells b_q_reg matched b_q by alias (reg-suffix)") && err.contains("CDC-MAP-ALIAS"), "{err}");
    let (_, out, err) = run(&["--rtl"]);
    assert!(out.contains("\"setup_timed\": true"), "without the ranges a_q[8] names nothing: {out}{err}");
    assert!(!err.contains("CDC-MAP-BIT"), "{err}");
    let (rc, _, err) = run(&["--ranges", ranges]);
    assert_eq!(rc, Some(2), "--ranges without --rtl is refused: {err}");
}
