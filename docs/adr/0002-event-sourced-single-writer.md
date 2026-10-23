# ADR 0002 — Event-sourced core with a single writer

**Status:** Accepted · 2026-09-03
**Applies to:** `engine/crates/domain`, `engine/crates/journal`, `engine/crates/book`

## Context

The platform this replaces puts Microsoft SQL Server in the trade path: an
order arrives, the server queries the database for the account's margin, writes
a row, and returns. That design is the reason the incumbent feels heavy under
load, and it caps order throughput at the database's transaction rate no matter
what language the server is written in.

It also produces the harder problem: with several threads reading and writing
shared account state, two orders can both pass a margin check against the same
free balance before either writes. The account ends up with negative equity and
no single line of code is wrong.

## Decision

**State is a fold over an append-only event stream, applied by one thread.**

1. **Events are the only truth.** Every state change is an `Event` appended to
   the journal. Any fact not expressible as an event does not exist, and any
   state not reachable by replaying from genesis is a bug.
2. **The journal is the database on the trade path.** The engine writes there
   and nowhere else. PostgreSQL is a downstream projection fed asynchronously
   from the same stream, and is never read while matching.
3. **One writer per book.** An `OrderBook` is owned by exactly one thread, which
   applies orders in a serialised loop. No locks, because there is nothing to
   contend over.
4. **Time is supplied, not read.** The engine never calls `SystemTime::now()`
   inside the matching loop. Timestamps arrive with the command and are
   journaled, so a replay reproduces them exactly.
5. **The wire format is hand-written and versioned.** `WIRE_VERSION` is part of
   every frame. Encoding is explicit rather than derived, because a derive
   macro's output can shift between releases of a dependency, and a regulator
   asking us to reproduce a 2029 trading day needs the same bytes back.

## Consequences

**Good.**

- Replay, audit and crash recovery are properties of the design rather than
  features somebody has to build.
- The two-orders-one-balance race is unreachable, not merely unlikely.
- CI can replay a recorded trading day and assert identical closing balances.
  That is the regression test that actually matters, and it is only possible
  because of points 3 and 4 above.
- A hard kill costs at most one event. `Journal::open` detects a torn final
  frame — short body, or a body that fails its CRC-32 — truncates it, and
  resumes.

**Costs we accept.**

- A single writer is a throughput ceiling per book. It is a high one: the
  matching loop is a few hundred nanoseconds and the constraint is one thread,
  not one core, since books shard cleanly by instrument.
- Every query becomes a projection to maintain. Reads never touch the engine.
- Schema evolution is migration of a wire format rather than `ALTER TABLE`.
  New event variants take new tags; tags are never reused or renumbered.

**Deliberately deferred.**

- **Journal segmentation.** `read_all` loads a whole file into memory. Fine for
  a day, wrong for a year. Segment rotation and snapshotting are P1.
- **fsync cadence.** `sync()` exists; the engine does not yet call it on a
  schedule. An fsync per order would cap throughput at the disk's IOPS, so the
  real policy is a bounded interval — and the window that opens must be a
  written, deliberate number rather than an accident.

## Alternatives rejected

**Database of record on the trade path.** The incumbent's design. Rejected for
the throughput ceiling and the race described above.

**Multi-threaded book with fine-grained locking.** Faster in a benchmark,
and the source of the exact class of bug we are trying to make unreachable.
Correctness of concurrent margin arithmetic is not something to relitigate at
three in the morning during a news spike.

**CRDTs or multi-master.** Order matching needs a total order over events. That
is the one thing these give up.
