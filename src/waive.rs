//! Waivers: findings a team has looked at and accepted, with the reason recorded.
//!
//! A checker that finds something real and gives no way to accept it gets switched off — and a
//! switched-off checker finds nothing at all. That is the whole argument for this file. Two
//! findings this engine reports cannot be judged from structure alone: a **multi-bit crossing**
//! is correct when the bus is gray-coded or handshake-qualified, and an **unsynchronized
//! crossing** is sometimes deliberate (a static configuration register written once before the
//! receiving domain is released). Neither is visible in a netlist. The design team knows; the
//! file is where they say so.
//!
//! # Format
//!
//! Blocks separated by blank lines, `key: value` inside, `#` starts a comment. Plain text so it
//! diffs in review, which is where a waiver should be argued about:
//!
//! ```text
//! # FIFO read pointer — reviewed with the async-FIFO design note
//! waive:      multibit
//! from:       core/wptr_gray_reg
//! to:         core/wptr_sync_reg
//! from_clock: clk_wr
//! to_clock:   clk_rd
//! reason:     gray-coded pointer; exactly one bit changes per transition
//! approver:   a.engineer
//! expires:    2027-01-01
//! ```
//!
//! `waive:` and `reason:` are required. A waiver with no reason is the thing that becomes a
//! permanent hole nobody can re-argue, so the parser refuses it rather than accepting a blank.
//!
//! `from` / `to` / `from_clock` / `to_clock` default to `*` and accept `*` (anything) or a
//! trailing `prefix*`. They are patterns because a hierarchical design waives by subtree; they
//! are *reported with a match count* because a pattern that silently covers more than intended
//! is how waivers rot.
//!
//! # Expiry
//!
//! Optional, `YYYY-MM-DD`, and **a lapsed waiver stops applying** — its findings come back. That
//! is the point of an expiry date, and it is the detail that separates a waiver from a
//! permanent exemption. Waivers with no expiry are counted in the report so their number stays
//! visible.
//!
//! Because expiry makes a run depend on the date, `--as-of YYYY-MM-DD` pins it — for a
//! reproducible sign-off run, and so this module's own tests do not start failing in 2027.

use crate::cdc::CdcReport;
use crate::rdc::RdcReport;

/// Which kind of finding a waiver covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaiveKind {
    /// An unsynchronized crossing (clock- or reset-domain). Never an `OK` one: there is
    /// nothing to accept about a finding that was not reported.
    Crossing,
    /// A multi-bit bus crossing.
    MultiBit,
    /// Either.
    Any,
}

impl WaiveKind {
    fn parse(s: &str) -> Option<WaiveKind> {
        match s {
            "crossing" => Some(WaiveKind::Crossing),
            "multibit" => Some(WaiveKind::MultiBit),
            "any" => Some(WaiveKind::Any),
            _ => None,
        }
    }
    fn covers(self, other: WaiveKind) -> bool {
        self == WaiveKind::Any || self == other
    }
}

#[derive(Debug, Clone)]
pub struct Waiver {
    pub kind: WaiveKind,
    pub from: String,
    pub to: String,
    pub from_clock: String,
    pub to_clock: String,
    pub reason: String,
    pub approver: Option<String>,
    /// `YYYY-MM-DD`, already validated, kept as written for the report.
    pub expires: Option<String>,
    /// Days since the epoch, for comparison.
    expires_days: Option<i64>,
    /// Line the block starts on — a waiver is reviewed in a file, so diagnostics name the line.
    pub line: usize,
}

impl Waiver {
    fn matches(&self, kind: WaiveKind, from: &str, to: &str, fc: &str, tc: &str) -> bool {
        self.kind.covers(kind)
            && pattern_matches(&self.from, from)
            && pattern_matches(&self.to, to)
            && pattern_matches(&self.from_clock, fc)
            && pattern_matches(&self.to_clock, tc)
    }
    fn lapsed(&self, today: i64) -> bool {
        self.expires_days.map(|e| e < today).unwrap_or(false)
    }
}

/// `*` matches anything, `prefix*` matches by prefix, anything else is exact.
fn pattern_matches(pat: &str, name: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pat == name,
    }
}

#[derive(Debug, Default)]
pub struct WaiverSet {
    pub waivers: Vec<Waiver>,
}

/// One finding that a waiver accounted for.
#[derive(Debug, Clone)]
pub struct Waived {
    pub what: String,
    pub reason: String,
    pub waiver_line: usize,
}

/// What applying a set of waivers did — including the parts that should make someone act.
#[derive(Debug, Default)]
pub struct WaiveOutcome {
    pub waived: Vec<Waived>,
    /// Waivers that matched nothing. The design changed or the finding was fixed; either way
    /// the waiver is now a claim about nothing, and dead waivers are how a file stops being read.
    pub stale: Vec<usize>,
    /// Waivers past their expiry date. **Not applied** — their findings are reported again.
    pub lapsed: Vec<usize>,
    /// Live waivers carrying no expiry date at all.
    pub no_expiry: usize,
}

#[derive(Debug)]
pub struct WaiveError(pub String);
impl std::fmt::Display for WaiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "waiver error: {}", self.0)
    }
}
impl std::error::Error for WaiveError {}

impl WaiverSet {
    pub fn load(path: &str) -> Result<WaiverSet, WaiveError> {
        let text = std::fs::read_to_string(path).map_err(|e| WaiveError(format!("{path}: {e}")))?;
        WaiverSet::parse(&text).map_err(|e| WaiveError(format!("{path}: {}", e.0)))
    }

    pub fn parse(text: &str) -> Result<WaiverSet, WaiveError> {
        let mut waivers = Vec::new();
        let mut block: Vec<(String, String, usize)> = Vec::new();
        let mut flush = |block: &mut Vec<(String, String, usize)>| -> Result<(), WaiveError> {
            if block.is_empty() {
                return Ok(());
            }
            waivers.push(build(block)?);
            block.clear();
            Ok(())
        };
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                flush(&mut block)?;
                continue;
            }
            let (k, v) = line.split_once(':').ok_or_else(|| {
                WaiveError(format!(
                    "line {}: expected 'key: value', got {line:?}",
                    i + 1
                ))
            })?;
            block.push((k.trim().to_lowercase(), v.trim().to_string(), i + 1));
        }
        flush(&mut block)?;
        Ok(WaiverSet { waivers })
    }

    /// The first live waiver covering a finding, or `None`.
    fn find(
        &self,
        kind: WaiveKind,
        from: &str,
        to: &str,
        fc: &str,
        tc: &str,
        today: i64,
    ) -> Option<usize> {
        self.waivers
            .iter()
            .position(|w| !w.lapsed(today) && w.matches(kind, from, to, fc, tc))
    }

    fn finish(&self, hits: &[usize], today: i64, out: &mut WaiveOutcome) {
        for (i, w) in self.waivers.iter().enumerate() {
            if w.lapsed(today) {
                out.lapsed.push(i);
            } else if !hits.contains(&i) {
                out.stale.push(i);
            } else if w.expires.is_none() {
                out.no_expiry += 1;
            }
        }
    }
}

fn build(block: &[(String, String, usize)]) -> Result<Waiver, WaiveError> {
    let line = block[0].2;
    let get = |k: &str| {
        block
            .iter()
            .find(|(a, _, _)| a == k)
            .map(|(_, v, _)| v.clone())
    };
    let kind = get("waive").ok_or_else(|| {
        WaiveError(format!(
            "line {line}: a waiver needs 'waive: crossing|multibit|any'"
        ))
    })?;
    let kind = WaiveKind::parse(&kind)
        .ok_or_else(|| WaiveError(format!("line {line}: unknown waive kind {kind:?}")))?;
    // A waiver with no reason cannot be reviewed, only inherited. Refuse it here rather than
    // let a design accumulate exemptions nobody can argue about.
    let reason = get("reason").filter(|r| !r.is_empty()).ok_or_else(|| {
        WaiveError(format!(
            "line {line}: a waiver needs a 'reason:' — it is what makes it reviewable"
        ))
    })?;
    let expires = get("expires").filter(|s| !s.is_empty());
    let expires_days = match &expires {
        Some(d) => Some(parse_date(d).ok_or_else(|| {
            WaiveError(format!(
                "line {line}: 'expires' must be YYYY-MM-DD, got {d:?}"
            ))
        })?),
        None => None,
    };
    let pat = |k: &str| {
        get(k)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "*".to_string())
    };
    Ok(Waiver {
        kind,
        from: pat("from"),
        to: pat("to"),
        from_clock: pat("from_clock"),
        to_clock: pat("to_clock"),
        reason,
        approver: get("approver").filter(|s| !s.is_empty()),
        expires,
        expires_days,
        line,
    })
}

/// `YYYY-MM-DD` → days since 1970-01-01, or `None` if it is not a date.
///
/// Only this direction is needed: comparisons happen in day numbers, and every date shown to a
/// reader is echoed back as the file wrote it.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Days from 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Today, in days since the epoch.
pub fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

/// Remove waived findings from a CDC report into the outcome.
///
/// Removed, not marked: a waived finding is one the team has already answered, and leaving it
/// in the violation list would mean the list no longer says what needs attention. The count and
/// the reason stay, in the outcome — accepting something is not the same as hiding it.
pub fn apply(report: &mut CdcReport, set: &WaiverSet, today: i64) -> WaiveOutcome {
    let mut out = WaiveOutcome::default();
    let mut hits: Vec<usize> = Vec::new();
    report.crossings.retain(|c| {
        if c.synchronized {
            return true; // not a finding; nothing to waive
        }
        match set.find(
            WaiveKind::Crossing,
            &c.from_flop,
            &c.to_flop,
            &c.from_domain,
            &c.to_domain,
            today,
        ) {
            Some(i) => {
                hits.push(i);
                out.waived.push(Waived {
                    what: format!(
                        "{} [{}] -> {} [{}]",
                        c.from_flop, c.from_domain, c.to_flop, c.to_domain
                    ),
                    reason: set.waivers[i].reason.clone(),
                    waiver_line: set.waivers[i].line,
                });
                false
            }
            None => true,
        }
    });
    report.multibit.retain(|m| {
        match set.find(
            WaiveKind::MultiBit,
            &m.bus_from,
            &m.bus_to,
            &m.from_domain,
            &m.to_domain,
            today,
        ) {
            Some(i) => {
                hits.push(i);
                out.waived.push(Waived {
                    what: format!(
                        "{}{} [{}] -> {} [{}] (multi-bit)",
                        m.bus_from,
                        m.bit_span(),
                        m.from_domain,
                        m.bus_to,
                        m.to_domain
                    ),
                    reason: set.waivers[i].reason.clone(),
                    waiver_line: set.waivers[i].line,
                });
                false
            }
            None => true,
        }
    });
    set.finish(&hits, today, &mut out);
    out
}

/// The same, for a reset-domain report. A reset crossing carries the same four names, so it
/// takes the same waivers — a team should not need two files and two syntaxes to accept two
/// findings of the same shape.
pub fn apply_rdc(report: &mut RdcReport, set: &WaiverSet, today: i64) -> WaiveOutcome {
    let mut out = WaiveOutcome::default();
    let mut hits: Vec<usize> = Vec::new();
    report.crossings.retain(|c| {
        if c.synchronized {
            return true;
        }
        match set.find(
            WaiveKind::Crossing,
            &c.from_flop,
            &c.to_flop,
            &c.from_domain,
            &c.to_domain,
            today,
        ) {
            Some(i) => {
                hits.push(i);
                out.waived.push(Waived {
                    what: format!(
                        "{} [{}] -> {} [{}]",
                        c.from_flop, c.from_domain, c.to_flop, c.to_domain
                    ),
                    reason: set.waivers[i].reason.clone(),
                    waiver_line: set.waivers[i].line,
                });
                false
            }
            None => true,
        }
    });
    set.finish(&hits, today, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: &str = "2026-08-11";

    fn set(text: &str) -> WaiverSet {
        WaiverSet::parse(text).expect("parse")
    }

    #[test]
    fn a_waiver_needs_a_kind_and_a_reason() {
        assert!(WaiverSet::parse("waive: multibit\n").is_err(), "no reason");
        assert!(WaiverSet::parse("reason: because\n").is_err(), "no kind");
        assert!(WaiverSet::parse("waive: nonsense\nreason: r\n").is_err());
        assert!(
            WaiverSet::parse("waive: multibit\nreason:\n").is_err(),
            "blank reason"
        );
        assert!(WaiverSet::parse("waive: any\nreason: r\n").is_ok());
    }

    #[test]
    fn blocks_are_separated_by_blank_lines_and_comments_are_ignored() {
        let s = set(
            "# a note\nwaive: crossing\nreason: one\n\nwaive: multibit\nreason: two  # trailing\n",
        );
        assert_eq!(s.waivers.len(), 2);
        assert_eq!(s.waivers[0].kind, WaiveKind::Crossing);
        assert_eq!(s.waivers[1].reason, "two");
        // Unset patterns default to "anything".
        assert_eq!(s.waivers[0].from, "*");
    }

    #[test]
    fn patterns_are_exact_or_prefix() {
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("core/*", "core/data_reg"));
        assert!(!pattern_matches("core/*", "other/data_reg"));
        assert!(pattern_matches("core/data_reg", "core/data_reg"));
        assert!(!pattern_matches("core/data_reg", "core/data_reg[0]"));
    }

    #[test]
    fn an_expiry_must_be_a_date_and_a_lapsed_waiver_stops_applying() {
        assert!(WaiverSet::parse("waive: any\nreason: r\nexpires: soon\n").is_err());
        assert!(WaiverSet::parse("waive: any\nreason: r\nexpires: 2026-13-01\n").is_err());
        let s = set("waive: any\nreason: r\nexpires: 2026-01-01\n");
        let today = parse_date(DAY).unwrap();
        assert!(s.waivers[0].lapsed(today), "January is behind us");
        assert!(
            s.find(WaiveKind::Crossing, "a", "b", "c1", "c2", today)
                .is_none(),
            "a lapsed waiver must not match — its findings come back"
        );
        // Valid through the day named, not up to it.
        let s = set(&format!("waive: any\nreason: r\nexpires: {DAY}\n"));
        assert!(!s.waivers[0].lapsed(today));
    }

    #[test]
    fn dates_convert_the_way_a_calendar_does() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-08-11"), Some(20676));
        assert!(parse_date("2026-02-29").unwrap() > parse_date("2026-02-28").unwrap());
        assert_eq!(parse_date("2026-8-11"), None, "zero-padded only");
        assert_eq!(parse_date("2026-08-11-01"), None);
    }
}
