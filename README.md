# vyges-cdc

Structural **clock-domain-crossing** check: a gate-level netlist, a Liberty, and
the clock definitions in — the list of domain crossings out, each flagged
synchronized or not.

Also **reset**-domain crossings (`vyges-cdc rdc`) — a flop asynchronously reset by one reset
feeding a flop reset by another. A single-clock design is CDC-clean by construction and can
still fail that way, so it is a separate report rather than a mode.

> **Vyges open EDA tools.** Commercial-grade silicon sign-off capability, built on
> open standards and plain file formats — and meant to be accessible to everyone,
> not only teams who can license a six-figure tool. `vyges-cdc` opens up CDC.

> **Stability: experimental (v0.1.0).** Crossing detection, 2-flop synchronizer
> recognition and multi-bit bus grouping are real and tested; reconvergence,
> gray-code/handshake *recognition* (a multi-bit crossing is reported, never judged
> safe), and data-stability are not yet covered (see **Current state**). Use it as an
> early structural lint, not a sign-off CDC tool.

## Reset-domain crossings (`rdc`)

When two flops are reset by **different asynchronous resets**, and those resets deassert
independently, the receiving flop can sample its input while the launching flop is still
settling out of reset. That is the same metastability failure CDC exists to find, from a
source no CDC check looks at — and it does not need two clocks.

```sh
vyges-cdc rdc netlist.v --lib sky130_fd_sc_hd__tt_025C_1v80.lib
```

No SDC. SDC has `create_clock`; it has no `create_reset`, so a reset domain is **structural**:
the net driving a flop's asynchronous reset pin, traced back through buffers and inverters to
whatever originates it — a primary input, or the output of a reset synchronizer. Two flops
share a domain when they trace to the same origin. Which pins are asynchronous resets comes
from the Liberty `ff` group's `clear`/`preset` expressions.

A **synchronous** reset is deliberately not a reset domain: it arrives as data, is timed like
any other path, and cannot cause a deassertion race. Counting it would report crossings
throughout every design, and a checker that cries wolf is one nobody runs.

### What `rdc` does not do

Stated plainly, because the gap between this and a commercial RDC tool is real:

- **It is gate-level.** Commercial RDC (e.g. Meridian RDC) runs on **RTL**, which is where the
  designer fixes it and where the intent is still visible. Ours runs after synthesis.
- **It does not prove protection.** A two-flop synchronizer is *recognized* structurally, the
  same as in the CDC path. That is pattern recognition, not a formal argument.
- **It does not analyse reset sequencing or ordering** — whether a destination is held in
  reset while its source deasserts is exactly the kind of protection it cannot see, so a
  reported crossing may be safe for a reason outside its view.
- **One domain per flop.** A cell whose `clear` and `preset` come from different resets is
  rare and not modelled.

### Validated on real netlists

Run against **nine synthesised sky130 blocks** (5 901 flops), four of them OpenTitan-derived,
at 0.05–0.25 s each. One design reported crossings between two independent top-level reset
ports; the finding was verified by hand against the netlist. Two designs correctly reported
**nothing to check** — one has no sequential cells, and one (a RISC-V core) resets
synchronously throughout, so no flop carries an asynchronous reset at all.

That last case is the one worth stating: a checker must be able to say *"I looked and there
was nothing to look at"*, distinctly from *"I looked and it was clean"*. The report leads with
the flop census for exactly that reason.

Use it as an early structural lint that costs nothing to run in CI, not as RDC sign-off.

## Why this exists

When a signal launched by a flop on one clock is captured by a flop on an
unrelated clock, the capture can go metastable. CDC analysis finds those crossings
and checks they are properly synchronized. It is a purely **structural**,
deterministic graph question — *which signals cross domains, and through what?* —
and notably a question a lockstep gate-level simulator **structurally cannot
answer** (it samples one consistent value per tick). That makes CDC a clean
complement to simulation, and squarely in the deterministic-Rust lane.

## How this is solved today

In production, CDC is a **commercial linter** (Questa CDC, Spyglass CDC, …) gated
behind major licenses. The open ecosystem is thin. `vyges-cdc` is a clean-room
Rust engine that reads the **same Liberty / Verilog / SDC** the rest of the Vyges
flow already uses — one toolset, one language.

## Use it

```sh
cargo build --release            # std-only beyond the shared parsers

vyges-cdc check design.v --lib cells.lib --sdc design.sdc            # -> crossings report
vyges-cdc check design.v --lib cells.lib --sdc design.sdc --json
vyges-cdc check design.v --lib cells.lib --sdc design.sdc --fail-on-violation  # exit 3
vyges-cdc check design.v --lib cells.lib --sdc design.sdc --fail-on-multibit   # exit 3
vyges-cdc check design.v --lib cells.lib --sdc design.sdc --waivers cdc-waivers.txt
# flags: --lib FILE · --sdc FILE · -o FILE · --json · --fail-on-violation ·
#        --fail-on-multibit · --waivers FILE · --as-of YYYY-MM-DD · -h · -V
```

Each **`create_clock`** in the SDC is a clock domain. The Liberty tells the engine
which cells are flops and which pins are clock / data / Q.

## How it works

- **Domain assignment** — every flop's clock pin is traced back (through clock
  buffers / inverters) to an SDC clock source; that source's name is the flop's
  domain. A clock declared on a **pin** (`[get_pins u_div/Q]` — how a generated
  clock is nearly always written) resolves to the net that pin drives, so it
  places flops rather than declaring a domain nothing can join.
- **Coverage, stated** — the report leads with a flop census: how many flops
  exist, how many were **placed** in a domain, and **which were not**. A flop
  whose clock does not trace to a declared clock is outside the analysis, so
  crossings into or out of it cannot be seen; a clean run over a partly-placed
  design says so instead of reading like a clean design.
- **Related clocks are not crossings** — `set_clock_groups` is the SDC's own
  statement about which clocks are asynchronous. Clocks in the same `-group` are
  related and their pairs are counted, not flagged; clocks in different groups
  are asynchronous. With no grouping declared, every differently-named clock is
  treated as asynchronous — the conservative reading.
- **Crossing detection** — for each capture flop, its data cone is walked back to
  the launching flops; any launch flop in a *different* domain is a crossing.
- **Clock muxes** — a flop whose clock pin is reachable from more than one declared
  clock is analysed in **every** one of them, and listed in the report. Taking the
  first clock found would miss the other crossing and make the verdict depend on the
  order the mux happened to be wired; whether the mux itself is glitch-free is a
  question this check does not ask.
- **Multi-bit crossings** — bits of one bus (`data_reg[0]`, `data_reg[1]`, …) crossing
  the same domain pair, each through **its own** synchronizer, are reported as a single
  finding. Every bit is individually safe and every bit reads `OK`; the bus is not,
  because the synchronizers settle on independent edges and the receiver can latch a
  combination that never existed at the source — a counter crossing as `0111 → 1000`
  can be read as `1111`. Reported, and gated only under `--fail-on-multibit`: a
  gray-coded or handshake-qualified bus is structurally identical and perfectly correct,
  and this check cannot tell them apart.
- **Synchronizer recognition** — a crossing is reported **OK** when it is a clean
  two-flop synchronizer: the source Q drives the capture flop's D **directly** (no
  logic), and that flop's Q feeds a **second** flop in the same domain. Otherwise
  it is a **violation** — either *no synchronizer* (a lone flop) or *logic on the
  CDC path* (combinational logic between domains, which a synchronizer must not
  have).

## Waivers

A checker that finds something real and gives no way to accept it gets switched off, and a
switched-off checker finds nothing at all. Two findings here cannot be judged from structure:
a **multi-bit crossing** is correct when the bus is gray-coded or handshake-qualified, and an
unsynchronized crossing is sometimes deliberate. The netlist does not know; the design team
does. `--waivers FILE` is where they say so, in plain text that diffs in review:

```text
# FIFO read pointer — reviewed with the async-FIFO design note
waive:      multibit
from:       core/wptr_gray_reg
to:         core/wptr_sync_reg
from_clock: clk_wr
to_clock:   clk_rd
reason:     gray-coded pointer; exactly one bit changes per transition
approver:   a.engineer
expires:    2027-01-01
```

Blocks separated by blank lines; `#` comments. `waive:` (`crossing` | `multibit` | `any`) and
`reason:` are required — **a waiver with no reason cannot be reviewed, only inherited**, so the
parser refuses it. The four name patterns default to `*` and accept `*` or `prefix*`. The same
file waives `rdc` findings, which carry the same four names.

What the report keeps saying, so accepting a finding never becomes hiding one:

- **what was waived**, with its reason and the line it came from — a run whose output does not
  show what was accepted can only be trusted, not reviewed;
- **lapsed waivers**, which are *not applied*: their findings come back. That is what an expiry
  is for, and it is the difference between a waiver and a permanent exemption;
- **stale waivers** that matched nothing — the finding was fixed or the design moved, and a file
  full of dead waivers stops being read;
- **how many live waivers carry no expiry at all.**

Expiry makes the answer depend on the date, which a sign-off run cannot have, so
`--as-of YYYY-MM-DD` pins it and a result reproduces exactly as reported.

## Current state (v0.1.0)

**Working & tested:** domain assignment (incl. tracing through clock buffers, and
pin-form clock sources), the flop census and unplaced-flop disclosure,
`set_clock_groups` relatedness, multi-clocked (muxed) flops, multi-bit bus grouping,
waivers with reason/approver/expiry (and lapsed/stale disclosure), cross-domain launch→capture detection through
arbitrary combinational cones, the canonical 2-flop synchronizer, and the
"combinational logic on a CDC path" violation. Text + `--json` reports; a
`--fail-on-violation` CI exit code.

**Depth reserved (honest):**

- **Multi-bit crossings are reported but cannot be judged.** What makes one safe —
  gray coding, or a handshake that qualifies when the bus may be sampled — is a
  property of the data and the protocol, not of the netlist, so a correct multi-bit
  crossing and a broken one look identical here. Hence the separate opt-in gate, and
  the waiver file for the ones you have reviewed.
- **Waiver policy is per-file, not per-organisation.** There is no shared waiver
  repository, approval workflow or org-wide expiry policy — the mechanism is here,
  the governance around it is not.
- a bus whose bits are **not** named `base[i]` cannot be grouped — after synthesis
  that suffix is the only evidence left that the bits belong together;
- only the **2-flop synchronizer** is recognized — handshake / FIFO / gray-code
  multi-bit crossings are reported as unsynchronized until those patterns are added;
- **reconvergence** (multiple synchronized signals recombining) is not yet checked;
- **divided / gated clocks** off a flop are not traced to a domain in v0 — declare
  them with `create_generated_clock` and they place normally; undeclared, their
  flops are reported as unplaced rather than silently skipped;
- glitch / data-stability and metastability-injection simulation are out of scope
  (structural only).

**Validation roadmap:** correlate the crossing set against an established CDC
linter on representative SoC blocks — the oracle-backed discipline the rest of Loom
uses.
