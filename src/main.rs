//! vyges-cdc CLI.
//!
//!   vyges-cdc check NETLIST --lib L.lib --sdc S.sdc [-o OUT] [--json] [--fail-on-violation]
//!
//! Reports every clock-domain crossing, flagging unsynchronized ones. Exit codes:
//! 0 clean · 1 runtime error · 2 usage · 3 unsynchronized crossing(s) found
//! (only with --fail-on-violation).

use std::process::exit;

use vyges_cdc::cdc::{self, CdcReport};
use vyges_cdc::{liberty::Lib, netlist, sdc::Sdc};

const USAGE: &str = "\
vyges-cdc — structural clock-domain-crossing check

usage:
  vyges-cdc check NETLIST --lib L.lib --sdc S.sdc [-o OUT] [--json] [--fail-on-violation]

flags:
  --lib FILE            Liberty (identifies flops + clock/data pins) — required
  --sdc FILE            SDC clock definitions (the domains) — required
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

fn render_text(r: &CdcReport) -> String {
    let mut s = String::new();
    let unsync = r.crossings.iter().filter(|c| !c.synchronized).count();
    s.push_str(&format!(
        "vyges-cdc — {} domain(s), {} crossing(s), {} unsynchronized\n",
        r.domains.len(),
        r.crossings.len(),
        unsync
    ));
    if r.crossings.is_empty() {
        s.push_str("  no clock-domain crossings.\n");
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

fn render_json(r: &CdcReport) -> String {
    let mut s = String::from("{\n");
    s.push_str(&format!("  \"domains\": {},\n", r.domains.len()));
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
    emit(
        &Event::new(
            "vyges-cdc",
            if viols == 0 {
                Severity::Info
            } else {
                Severity::Warn
            },
            format!(
                "cdc check complete: {} crossing(s), {viols} unsynchronized",
                r.crossings.len()
            ),
        )
        .with_code("CDC-DONE"),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--describe") {
        // Machine-readable description of `check` for tooling that drives it.
        const DESCRIBE: &str = r#"{
  "name": "cdc",
  "summary": "structural clock-domain-crossing check",
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
  "artifacts": [ { "role": "cdc_report", "from_arg": "out" } ],
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
    if args[0] != "check" {
        eprintln!("error: unknown command {:?}\n{USAGE}", args[0]);
        exit(2);
    }
    let Some(net) = args.get(1).filter(|a| !a.starts_with('-')) else {
        eprintln!("error: `check` needs a NETLIST path\n{USAGE}");
        exit(2);
    };
    let (Some(libp), Some(sdcp)) = (opt(&args, "--lib"), opt(&args, "--sdc")) else {
        eprintln!("error: `check` needs --lib and --sdc\n{USAGE}");
        exit(2);
    };

    let nl = netlist::load(net).unwrap_or_else(|e| die(&format!("{net}: {e}")));
    let lib = Lib::load(&libp).unwrap_or_else(|e| die(&format!("{libp}: {e}")));
    let sdc = Sdc::load(&sdcp).unwrap_or_else(|e| die(&format!("{sdcp}: {e}")));

    let report = cdc::analyze(&nl, &lib, &sdc).unwrap_or_else(|e| die(&e));
    emit_cdc_events(&report);
    let json = args.iter().any(|a| a == "--json");
    let text = if json {
        render_json(&report)
    } else {
        render_text(&report)
    };
    match opt(&args, "-o") {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, &text) {
                die(&format!("{p}: {e}"));
            }
        }
        None => print!("{text}"),
    }
    let unsync = report.crossings.iter().filter(|c| !c.synchronized).count();
    if args.iter().any(|a| a == "--fail-on-violation") && unsync > 0 {
        exit(3);
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1);
}
