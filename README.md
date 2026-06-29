# vyges-cdc

Structural **clock-domain-crossing** check: a gate-level netlist, a Liberty, and
the clock definitions in — the list of domain crossings out, each flagged
synchronized or not.

> **Vyges open EDA tools.** Commercial-grade silicon sign-off capability, built on
> open standards and plain file formats — and meant to be accessible to everyone,
> not only teams who can license a six-figure tool. `vyges-cdc` opens up CDC.

> **Stability: experimental (v0.1.0).** Crossing detection and 2-flop synchronizer
> recognition are real and tested; reconvergence, gray-code/handshake recognition,
> and data-stability are not yet covered (see **Current state**). Use it as an
> early structural lint, not a sign-off CDC tool.

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
  domain.
- **Crossing detection** — for each capture flop, its data cone is walked back to
  the launching flops; any launch flop in a *different* domain is a crossing.
- **Synchronizer recognition** — a crossing is reported **OK** when it is a clean
  two-flop synchronizer: the source Q drives the capture flop's D **directly** (no
  logic), and that flop's Q feeds a **second** flop in the same domain. Otherwise
  it is a **violation** — either *no synchronizer* (a lone flop) or *logic on the
  CDC path* (combinational logic between domains, which a synchronizer must not
  have).

## Current state (v0.1.0)

**Working & tested:** domain assignment (incl. tracing through clock buffers),
cross-domain launch→capture detection through arbitrary combinational cones, the
canonical 2-flop synchronizer, and the "combinational logic on a CDC path"
violation. Text + `--json` reports; a `--fail-on-violation` CI exit code.

**Depth reserved (honest):**

- only the **2-flop synchronizer** is recognized — handshake / FIFO / gray-code
  multi-bit crossings are reported as unsynchronized until those patterns are added;
- **reconvergence** (multiple synchronized signals recombining) is not yet checked;
- **divided / gated clocks** off a flop are not traced to a domain in v0;
- glitch / data-stability and metastability-injection simulation are out of scope
  (structural only).

**Validation roadmap:** correlate the crossing set against an established CDC
linter on representative SoC blocks — the oracle-backed discipline the rest of Loom
uses.
