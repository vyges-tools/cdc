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

> **Stability: experimental (v0.1.0).** Crossing detection and 2-flop synchronizer
> recognition are real and tested; reconvergence, gray-code/handshake recognition,
> and data-stability are not yet covered (see **Current state**). Use it as an
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
# flags: --lib FILE · --sdc FILE · -o FILE · --json · --fail-on-violation · -h · -V
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
- **Synchronizer recognition** — a crossing is reported **OK** when it is a clean
  two-flop synchronizer: the source Q drives the capture flop's D **directly** (no
  logic), and that flop's Q feeds a **second** flop in the same domain. Otherwise
  it is a **violation** — either *no synchronizer* (a lone flop) or *logic on the
  CDC path* (combinational logic between domains, which a synchronizer must not
  have).

## Current state (v0.1.0)

**Working & tested:** domain assignment (incl. tracing through clock buffers, and
pin-form clock sources), the flop census and unplaced-flop disclosure,
`set_clock_groups` relatedness, cross-domain launch→capture detection through
arbitrary combinational cones, the canonical 2-flop synchronizer, and the
"combinational logic on a CDC path" violation. Text + `--json` reports; a
`--fail-on-violation` CI exit code.

**Depth reserved (honest):**

- **A multi-bit crossing is not recognized as one.** Each bit of a bus with its own
  two-flop synchronizer is individually safe and reported `OK`, but the chains
  resolve independently, so the receiver can latch a combination that never existed
  at the source. Grouping bits of one bus into a single crossing is the next thing
  to build here, and until it exists a per-bit-synchronized bus reads as clean.
- **A flop clocked by a mux takes one of its clocks**, whichever the netlist wires
  first — so the other crossing is missed, and the answer depends on connection
  order. Tracing all reachable sources is the fix.
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
