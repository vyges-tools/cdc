//! vyges-cdc CLI.
//!
//!   vyges-cdc check NETLIST --lib L.lib --sdc S.sdc [-o OUT] [--json] [--fail-on-violation]
//!
//! Reports every clock-domain crossing, flagging unsynchronized ones. Exit codes:
//! 0 clean · 1 runtime error · 2 usage · 3 unsynchronized crossing(s) found
//! (only with --fail-on-violation).

use std::process::exit;

use vyges_cdc::cdc::{self, CdcReport};
use vyges_cdc::rdc;
use vyges_cdc::{liberty::Lib, netlist, sdc::Sdc};

const USAGE: &str = "\
vyges loom cdc — structural clock- and reset-domain-crossing checks

usage:
  vyges loom cdc check NETLIST --lib L.lib --sdc S.sdc [-o OUT] [--json] [--fail-on-violation]
  vyges loom cdc rdc   NETLIST --lib L.lib           [-o OUT] [--json] [--fail-on-violation]

`check` finds CLOCK-domain crossings; `rdc` finds RESET-domain crossings — a flop
asynchronously reset by one reset feeding a flop reset by another. A single-clock design is
CDC-clean by construction and can still fail `rdc`, so they are separate reports. `rdc` needs
no SDC: reset domains are structural, traced from the Liberty ff group's clear/preset pins.

flags:
  --lib FILE            Liberty (identifies flops + clock/data/reset pins) — required
  --sdc FILE            SDC clock definitions (the domains) — required by `check`
  -o FILE               write the report to FILE (default: stdout)
  --json                machine-readable JSON instead of text
  --fail-on-violation   exit 3 if any unsynchronized crossing is found (CI gate)
  --describe            print a machine-readable JSON description of the command
  -h, --help · -V, --version
";

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// A JSON array of quoted strings. Instance names come from a netlist, where an escaped
/// identifier can legally carry a backslash — which would otherwise end the JSON string early.
fn jlist(v: &[String]) -> String {
    v.iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_text(r: &CdcReport) -> String {
    let mut s = String::new();
    let unsync = r.crossings.iter().filter(|c| !c.synchronized).count();
    // Lead with the census, the way `rdc` does. Without it "0 crossings" over a design whose
    // flops were never placed in a domain reads exactly like a clean result.
    s.push_str(&format!(
        "vyges-cdc — {} flop(s) ({} placed, {} unplaced), {} domain(s), \
         {} crossing(s), {} unsynchronized\n",
        r.flops_total,
        r.flop_domain.len(),
        r.unplaced.len(),
        r.domains.len(),
        r.crossings.len(),
        unsync
    ));
    if !r.unplaced.is_empty() {
        s.push_str(&format!(
            "  [warn] {} flop(s) are NOT in the analysis — their clock does not trace to a\n\
             \x20        declared clock (a divided/gated clock off a flop, or an undeclared\n\
             \x20        clock port). Crossings into or out of them cannot be seen:\n",
            r.unplaced.len()
        ));
        for f in r.unplaced.iter().take(10) {
            s.push_str(&format!("           {f}\n"));
        }
        if r.unplaced.len() > 10 {
            s.push_str(&format!(
                "           … and {} more\n",
                r.unplaced.len() - 10
            ));
        }
    }
    if r.related_skipped > 0 {
        s.push_str(&format!(
            "  note: {} pair(s) cross clocks the SDC declares related (set_clock_groups) — \
             timed, not crossed.\n",
            r.related_skipped
        ));
    }
    if r.crossings.is_empty() {
        // Which kind of nothing this is. Three of them are distinguishable and they mean very
        // different things to the person reading a passing run.
        s.push_str(if r.flops_total == 0 {
            "  nothing to check: this netlist has no sequential cells.\n"
        } else if r.flop_domain.is_empty() {
            "  nothing was checked: no flop could be placed in a declared clock domain.\n"
        } else if r.unplaced.is_empty() {
            "  no clock-domain crossings.\n"
        } else {
            "  no clock-domain crossings among the flops that were placed.\n"
        });
        return s;
    }
    for c in r.crossings.iter().take(200) {
        let tag = if c.synchronized {
            "OK   (2-flop sync)"
        } else if c.through_logic {
            "VIOL (logic on CDC path)"
        } else {
            "VIOL (no synchronizer)"
        };
        s.push_str(&format!(
            "  {} [{}] → {} [{}]   {}\n",
            c.from_flop, c.from_domain, c.to_flop, c.to_domain, tag
        ));
    }
    s
}

fn render_rdc_text(r: &rdc::RdcReport) -> String {
    let mut s = String::new();
    let unsync = r.crossings.iter().filter(|c| !c.synchronized).count();
    s.push_str(&format!(
        "vyges-cdc (rdc) — {} flop(s) ({} async-reset, {} without), \
         {} reset domain(s), {} crossing(s), {} unsynchronized\n",
        r.seq_flops,
        r.seq_flops - r.unreset_flops,
        r.unreset_flops,
        r.domains.len(),
        r.crossings.len(),
        unsync
    ));
    if r.crossings.is_empty() {
        // Say which kind of nothing this is. A design with no async-reset flops has nothing to
        // check, and that must not read the same as a design that was checked and came back
        // clean — a real crossbar netlist made exactly that mistake possible.
        s.push_str(if r.seq_flops == r.unreset_flops {
            "  nothing to check: no flop in this netlist has an asynchronous reset.\n"
        } else {
            "  no reset-domain crossings.\n"
        });
        return s;
    }
    for c in r.crossings.iter().take(200) {
        let tag = if c.synchronized {
            "OK   (2-flop sync)"
        } else if c.through_logic {
            "VIOL (logic on RDC path)"
        } else {
            "VIOL (no synchronizer)"
        };
        s.push_str(&format!(
            "  {} [{}] → {} [{}]   {}\n",
            c.from_flop, c.from_domain, c.to_flop, c.to_domain, tag
        ));
    }
    s
}

fn render_rdc_json(r: &rdc::RdcReport) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"reset_domains\": {},\n", r.domains.len()));
    s.push_str(&format!("  \"crossings\": {},\n", r.crossings.len()));
    s.push_str(&format!("  \"flops\": {},\n", r.seq_flops));
    s.push_str(&format!("  \"unreset_flops\": {},\n", r.unreset_flops));
    s.push_str(&format!(
        "  \"unsynchronized\": {},\n",
        r.crossings.iter().filter(|c| !c.synchronized).count()
    ));
    s.push_str("  \"items\": [\n");
    for (i, c) in r.crossings.iter().enumerate() {
        let comma = if i + 1 < r.crossings.len() { "," } else { "" };
        s.push_str(&format!(
            "    {{\"from\": \"{}\", \"to\": \"{}\", \"from_domain\": \"{}\", \"to_domain\": \"{}\", \"synchronized\": {}, \"through_logic\": {}}}{}\n",
            c.from_flop, c.to_flop, c.from_domain, c.to_domain, c.synchronized, c.through_logic, comma
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

/// The causal trail for a reset-domain run. Same shape as the CDC one — `RDC-<KIND>` is the
/// clustering key, the flops and their reset domains are the co-ref keys.
fn emit_rdc_events(r: &rdc::RdcReport) {
    use vyges_events::{emit, Event, Severity};
    let mut viols = 0usize;
    for c in &r.crossings {
        if c.synchronized {
            continue;
        }
        viols += 1;
        let kind = if c.through_logic { "LOGIC" } else { "UNSYNC" };
        let detail = if c.through_logic {
            "combinational logic on reset-domain-crossing path"
        } else {
            "reset-domain crossing with no synchronizer"
        };
        emit(
            &Event::new(
                "vyges-cdc",
                Severity::Warn,
                format!(
                    "{}: {} [{}] → {} [{}]",
                    detail, c.from_flop, c.from_domain, c.to_flop, c.to_domain
                ),
            )
            .with_code(format!("RDC-{kind}"))
            .with_objects(vec![
                format!("flop:{}", c.from_flop),
                format!("flop:{}", c.to_flop),
                format!("reset:{}", c.from_domain),
                format!("reset:{}", c.to_domain),
            ]),
        );
    }
    emit(
        &Event::new(
            "vyges-cdc",
            if viols > 0 {
                Severity::Warn
            } else {
                Severity::Info
            },
            format!(
                "checked {} reset domain(s): {} crossing(s), {} unsynchronized",
                r.domains.len(),
                r.crossings.len(),
                viols
            ),
        )
        .with_code("RDC-DONE"),
    );
}

fn render_json(r: &CdcReport) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"domains\": {},\n", r.domains.len()));
    s.push_str(&format!("  \"flops\": {},\n", r.flops_total));
    s.push_str(&format!("  \"flops_placed\": {},\n", r.flop_domain.len()));
    s.push_str(&format!(
        "  \"related_pairs_skipped\": {},\n",
        r.related_skipped
    ));
    // Named, not just counted: a consumer deciding whether to trust a clean run needs to know
    // *which* flops were outside it.
    s.push_str(&format!("  \"unplaced\": [{}],\n", jlist(&r.unplaced)));
    s.push_str(&format!("  \"crossings\": {},\n", r.crossings.len()));
    s.push_str(&format!(
        "  \"unsynchronized\": {},\n",
        r.crossings.iter().filter(|c| !c.synchronized).count()
    ));
    s.push_str("  \"items\": [\n");
    for (i, c) in r.crossings.iter().enumerate() {
        let comma = if i + 1 < r.crossings.len() { "," } else { "" };
        s.push_str(&format!(
            "    {{\"from\": \"{}\", \"to\": \"{}\", \"from_domain\": \"{}\", \"to_domain\": \"{}\", \"synchronized\": {}, \"through_logic\": {}}}{}\n",
            c.from_flop, c.to_flop, c.from_domain, c.to_domain, c.synchronized, c.through_logic, comma
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

/// Emit the vyges-events causal trail — one event per unsynchronized crossing + a
/// completion event. Written to stderr (the default sink) so it never mixes with the
/// report (stdout / -o). `code` (CDC-<KIND>) is the clustering key; `objects` (the
/// launch/capture nets and their clock domains) are the cross-stage co-ref keys.
fn emit_cdc_events(r: &CdcReport) {
    use vyges_events::{emit, Event, Severity};
    let mut viols = 0usize;
    for c in &r.crossings {
        if c.synchronized {
            continue; // a clean 2-flop synchronizer is not a violation
        }
        viols += 1;
        let kind = if c.through_logic { "LOGIC" } else { "UNSYNC" };
        let detail = if c.through_logic {
            "combinational logic on clock-domain-crossing path"
        } else {
            "no synchronizer on clock-domain crossing"
        };
        emit(
            &Event::new(
                "vyges-cdc",
                Severity::Warn,
                format!(
                    "{}: {} [{}] -> {} [{}]",
                    detail, c.from_flop, c.from_domain, c.to_flop, c.to_domain
                ),
            )
            .with_code(format!("CDC-{kind}"))
            .with_objects(vec![
                format!("net:{}", c.from_flop),
                format!("net:{}", c.to_flop),
                format!("clock:{}", c.from_domain),
                format!("clock:{}", c.to_domain),
            ]),
        );
    }
    // Coverage before verdict: a clean run over a partly-placed design is a weaker statement
    // than a clean run over all of it, and only this event carries the difference to whatever
    // is reading the trail.
    if !r.unplaced.is_empty() {
        emit(
            &Event::new(
                "vyges-cdc",
                Severity::Warn,
                format!(
                    "{} of {} flop(s) are outside the analysis: their clock does not trace to a \
                     declared clock (divided/gated clock off a flop, or an undeclared clock \
                     port). Crossings involving them are not visible to this check",
                    r.unplaced.len(),
                    r.flops_total
                ),
            )
            .with_code("CDC-UNPLACED")
            .with_objects(
                r.unplaced
                    .iter()
                    .take(50)
                    .map(|f| format!("instance:{f}"))
                    .collect(),
            ),
        );
    }
    emit(
        &Event::new(
            "vyges-cdc",
            if viols == 0 {
                Severity::Info
            } else {
                Severity::Warn
            },
            format!(
                "cdc check complete: {} of {} flop(s) placed in {} domain(s); {} crossing(s), \
                 {viols} unsynchronized",
                r.flop_domain.len(),
                r.flops_total,
                r.domains.len(),
                r.crossings.len()
            ),
        )
        .with_code("CDC-DONE"),
    );
}

/// Add `"report_path"` to a `--json` payload so the result says where its report landed.
///
/// String surgery rather than a JSON round-trip because this crate is std-only. Inserting
/// after the opening brace keeps every existing field untouched; an empty object gets no
/// trailing comma.
fn with_report_path(json: &str, path: Option<&str>) -> String {
    let (Some(p), Some(rest)) = (path, json.trim_start().strip_prefix('{')) else {
        return json.to_string();
    };
    let esc = p.replace('\\', "\\\\").replace('"', "\\\"");
    let sep = if rest.trim_start().starts_with('}') {
        ""
    } else {
        ","
    };
    format!("{{\"report_path\": \"{esc}\"{sep}{rest}")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--describe") {
        // Machine-readable description of `check` for tooling that drives it.
        const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "name": "cdc",
  "summary": "structural clock-domain-crossing check",
  "maturity": "structured",
  "provenance_limitations": [
      "input_hash covers the argument vector, not the content of the netlist, Liberty or SDC it names.",
      "Liberty `include` files are not enumerated."
  ],
  "invocation": {
    "args_template": ["check", "{netlist}", "--lib", "{lib}", "--sdc", "{sdc}"],
    "optional": [
      { "arg": "out", "flag": "-o" }
    ],
    "emits_json": true
  },
  "inputs": {
    "type": "object",
    "required": ["netlist", "lib", "sdc"],
    "properties": {
      "netlist": { "type": "string", "description": "gate-level netlist to analyze" },
      "lib": { "type": "string", "description": "Liberty file identifying flops and clock/data pins" },
      "sdc": { "type": "string", "description": "SDC file defining clock domains" },
      "out": { "type": "string", "description": "write the report to this file instead of stdout" }
    }
  },
  "artifacts": [ { "role": "cdc_report", "field": "report_path" } ],
  "assertion": {
    "id": "cdc-synchronized",
    "field": "unsynchronized",
    "pass_when": { "eq": 0 }
  }
}
"#;
        print!("{DESCRIBE}");
        return;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("vyges-cdc {}", vyges_cdc::VERSION);
        return;
    }
    if args[0] != "check" && args[0] != "rdc" {
        eprintln!("error: unknown command {:?}\n{USAGE}", args[0]);
        exit(2);
    }
    let reset_mode = args[0] == "rdc";
    let Some(net) = args.get(1).filter(|a| !a.starts_with('-')) else {
        eprintln!("error: `check` needs a NETLIST path\n{USAGE}");
        exit(2);
    };
    let Some(libp) = opt(&args, "--lib") else {
        eprintln!("error: needs --lib\n{USAGE}");
        exit(2);
    };
    // `rdc` takes no SDC: reset domains are structural, not declared. Accepting one and
    // ignoring it would imply it changed the answer.
    if !reset_mode && opt(&args, "--sdc").is_none() {
        eprintln!("error: `check` needs --sdc\n{USAGE}");
        exit(2);
    }

    let nl = netlist::load(net).unwrap_or_else(|e| die(&format!("{net}: {e}")));
    let lib = Lib::load(&libp).unwrap_or_else(|e| die(&format!("{libp}: {e}")));

    let json = args.iter().any(|a| a == "--json");
    let (text, unsync) = if reset_mode {
        let report = rdc::analyze(&nl, &lib).unwrap_or_else(|e| die(&e));
        emit_rdc_events(&report);
        let n = report.crossings.iter().filter(|c| !c.synchronized).count();
        let t = if json {
            with_report_path(&render_rdc_json(&report), opt(&args, "-o").as_deref())
        } else {
            render_rdc_text(&report)
        };
        (t, n)
    } else {
        let sdcp = opt(&args, "--sdc").expect("checked above");
        let sdc = Sdc::load(&sdcp).unwrap_or_else(|e| die(&format!("{sdcp}: {e}")));
        let report = cdc::analyze(&nl, &lib, &sdc).unwrap_or_else(|e| die(&e));
        emit_cdc_events(&report);
        let n = report.crossings.iter().filter(|c| !c.synchronized).count();
        let t = if json {
            with_report_path(&render_json(&report), opt(&args, "-o").as_deref())
        } else {
            render_text(&report)
        };
        (t, n)
    };
    match opt(&args, "-o") {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, &text) {
                die(&format!("{p}: {e}"));
            }
            eprintln!("wrote {p}");
            // `-o` writes the report; the machine payload still goes to stdout, so asking
            // for the file does not cost the caller the parsed result.
            if json {
                print!("{text}");
            }
        }
        None => print!("{text}"),
    }
    if args.iter().any(|a| a == "--fail-on-violation") && unsync > 0 {
        exit(3);
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}
