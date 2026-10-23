# ADR 0001 — Money is fixed-point integers, never floats

**Status:** Accepted · 2026-09-03
**Applies to:** `engine/crates/money`

## Context

Prices, quantities, balances and P&L all need decimal arithmetic. The obvious
choice — `f64` — is wrong in a way that does not show up in testing and does
show up in reconciliation months later.

`f64` cannot represent `1.08532` exactly. Each arithmetic step introduces an
error around 1e-16 relative. On a single trade that is invisible. Across a
million fills, an accumulating balance, and a daily rollover, the errors
compound in a direction that depends on the order operations happened to run
in — which means two replays of the same trading day can disagree.

The failure mode is not "the number looks slightly wrong". It is:

- The internal ledger and the liquidity provider's statement differ by an
  amount nobody can explain or attribute to a trade.
- Journal replay produces different closing balances than the live run did,
  destroying the audit trail's value.
- A margin level sits a hair either side of the stop-out threshold, and whether
  a client gets liquidated depends on floating-point noise.

## Decision

Every monetary value is a scaled integer.

| Type | Decimal places | Storage |
|---|---|---|
| `Price` | 9 | `i64` |
| `Qty` | 8 | `i64` |
| `Money` | 9 | `i128` |

Rules that follow from it:

1. **No `f32`/`f64` in the engine workspace.** Enforced by
   `#![deny(clippy::float_arithmetic)]` in each crate and by the absence of any
   float in the public API.
2. **Every lossy operation rounds explicitly, half away from zero.** Rust's `/`
   truncates toward zero. On a signed P&L that means every rounded fraction
   favours whoever is on the positive side — systematically the house. That is
   both wrong and, in most jurisdictions, a finding.
3. **Every fallible operation returns `Result`.** `checked_add`, not `+`. The
   crate deliberately does *not* implement `std::ops::Add`, because doing so
   would make `a + b` compile and wrap silently.
4. **`Money` is `i128`.** `price × qty` at institutional size overflows 64 bits.
   `108532.00` at nine decimals is already 1.08e14; a hundred-lot notional
   intermediate is 1.08e22.

## Consequences

**Good.** Arithmetic is exact and deterministic. The same journal replays to
byte-identical balances on any machine, which is what makes the CI replay gate
meaningful. Reconciliation breaks are always real, never numerical noise.

**Costs we accept.** The code is more verbose — `a.add(b)?` rather than `a + b`.
Callers must decide what to do on overflow rather than ignoring it. Division
needs an explicit rounding decision at every site. All three are the point.

**Not solved by this.** Currency conversion still loses precision, because a
five-decimal rate is not an exact reciprocal of its inverse. That residual is
the rate's precision, not our arithmetic, and
`a_currency_round_trip_loses_only_the_rate_s_own_precision` asserts it stays
bounded by one tick of the rate.

## Alternatives rejected

**`f64` with rounding at the boundary.** The usual defence — "we round to two
decimals before displaying" — does not help, because the error is already baked
into the accumulated balance before display.

**`rust_decimal` or a similar crate.** Correct, and a reasonable choice in
general. Rejected here because the engine workspace holds a zero-dependency
line (see the workspace manifest), and because a fixed 9-decimal scale is
faster and simpler than arbitrary precision for the one job we need.

**Storing minor units only (cents).** Insufficient precision. FX pips are the
fifth decimal, and swap accrual needs finer still.
