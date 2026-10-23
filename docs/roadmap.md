# Roadmap

The goal is everything Cubex-v2 does, in Feraro. [docs/cubex-gap.md](cubex-gap.md)
is the inventory; this is the order and the state.

Kept in the repo rather than in a chat so it survives, and so anybody can
see what is done without asking.

## How a row gets done

Every row ends the same way: tests beside the code, the full suite green,
clippy clean, verified against a running gateway, and pushed. A feature that
is not tested is not done, because on a platform that holds money the first
person to find the bug must not be a client.

Rows marked **engine** change the journal. Those take a new event tag and
leave the old one decoding, because a journal written last week must still
replay. That is the cost of this architecture and it is paid deliberately.

## Done

- [x] **Spread markup** — per instrument, applied once on the way in so the
      client deals at the price they were shown. The fill journals what the
      house earned. *(engine)*
- [x] **Payment methods and deposit claims** — crypto, UPI, bank. The desk
      confirms money arrived; the credit is an ordinary journaled deposit.
- [x] **Server selection at sign-in** — one build, several deployments.
- [x] **One demo per login**, thirty days. *(engine)*
- [x] **Roles** — client, manager, admin, with the book scoped to a manager.
- [x] **Deployment** — containers, automatic TLS, health endpoint, backup and
      restore with a tested replay.
- [x] **Alert sounds** — nine, synthesised.
- [x] **Load tested** — eight traders, live prices, books still net to zero.

## Next, in order

Ordered by what a broker needs to operate, not by size.

- [ ] **Commission per lot** *(engine)* — the second half of revenue. The
      ledger already has the book; the fill needs to carry it and the fold
      needs to post it.
- [ ] **Withdrawals** — clients can pay in but cannot ask to take money out.
      Saved withdrawal methods, a request queue, desk approval.
- [ ] **Transfer between accounts** *(engine)* — one event carrying both
      legs, so a crash cannot leave half a transfer.
- [ ] **Two-factor for desk logins** — TOTP. A login that can move money is
      single-factor today.
- [ ] **Partial close** *(engine)* and **modify an open trade**.
- [ ] **Trailing stop** *(engine)* and **expiry on resting orders**.
- [ ] **Per-account risk settings** — leverage, margin-call level, maximum
      lot, swap-free. *(engine, mostly)*
- [ ] **Email** — statements, margin calls, deposits confirmed. A margin call
      nobody is looking at is not a margin call.
- [ ] **Symbol administration** — replace the compiled catalogue with one the
      desk edits: enable, digits, swap, commission, per-symbol markup.
- [ ] **Trade groups** — a markup and swap multiplier per group of accounts,
      overridable per account.
- [ ] **Reporting read model** — project the journal into Postgres for
      queries. The journal stays authoritative; the projection is rebuilt by
      replay, so it can never be the thing that is wrong.
- [ ] **Price alerts**, **notifications**, **push**.
- [ ] **KYC document upload** and review.
- [ ] **Copy trading** — master and follower with a lot ratio.
- [ ] **Referral**.
- [ ] **Journal compaction** — nothing archives segments a snapshot has made
      redundant, so the disk grows forever. A retention policy first.

## Deliberately last, or not at all

- **Multi-tenancy.** Cubex has a superadmin tier over tenants. Feraro answers
  the same question with one platform per server, chosen at sign-in, which
  covers several brands. A tenant layer *inside* one server is a change to
  every account id, journal and user store — worth doing only if one operator
  must run several brands from one screen.
- **A payment provider.** Needs a licence and a commercial relationship
  before it needs code.
- **Backdating a money movement.** Cubex lets an operator set the applied
  date. Feraro must not rewrite journal time — order is the property every
  balance rests on. The right version is a *value date* recorded beside the
  real one, and it waits for a string in the wire format.
- **Deleting a client.** Ids are never reused and the journal is append-only.
  What can be built is anonymising the login while the trading record stands.

## What is still missing before real money

Unchanged, and none of it is code:

A licence. KYC documents actually collected and checked. Client money held
separately. A market-data licence — the prices come from Binance, Kraken and
Twelve Data, and `EURUSDT` is a stablecoin pair standing in for EURUSD.

Technically: backups are tested but nothing copies them off the machine yet,
and the journal has no compaction.
