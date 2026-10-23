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
- [x] **Commission per lot** — charged on opening and on closing, posted to
      the house book. *(engine)*
- [x] **Withdrawals** — saved payout methods, a request queue, desk approval,
      and the three refusals that happen before it reaches the desk.
- [x] **One queue for every money movement** — deposit, withdrawal, bonus in
      and out, insurance in and out, all approved the same way.
- [x] **Insurance** — the client's own money, set aside where the market
      cannot reach it. Not bonus: it supports no margin. *(engine)*
- [x] **Cubex's UI language** — cards, instrument marks, price pills,
      segmented controls, in pearl rather than black.
- [x] **Screens for all of it** — the three features above were wire-only.
      Now: one **Requests** queue on the desk for every kind of movement,
      with per-kind counts and a confirmation that names what the operator
      must have done; a **Pricing** panel for markup and commission beside
      the spread they produce; the client's **wallet** — pay in, withdraw,
      and the payout book — on the desktop and on the phone; and insurance
      raised from an account's own menu into the same queue.
- [x] **Transfer between accounts** *(engine)* — one event carrying both
      legs, so no replay can land on a state where the money has left one
      account and arrived nowhere. Demo and live never mix; the bonus
      cannot be parked elsewhere; both legs appear on their statements with
      one sequence number. A client moves their own money without asking;
      money between two people is an administrator's.
- [x] **Two-factor for desk logins** — TOTP, RFC 6238, written out rather
      than pulled in: SHA-1, HMAC and base32 are two hundred lines, and a
      dependency on the authentication path is one whose every future
      version has to be read. Pinned to the published vectors. A used code
      is refused for the rest of its window, including across a restart,
      so a one-time password is used once. Offered to desk logins only — a
      client cannot move money without the desk agreeing — and the desk
      screen says so until it is set up.

## Next, in order

Ordered by what a broker needs to operate, not by size.

- [ ] **Make two-factor compulsory** for desk logins, behind a setting. It
      is offered and nagged about today; nothing refuses a desk login that
      has not set it up, because the switch that would is also the switch
      that locks an operator out of their own platform on upgrade.
- [ ] **A QR code for enrolment.** The secret is typed in by hand today.
      Fetching a QR tells an image service the secret, and a hand-written
      encoder cannot be tested here without also writing a decoder — so it
      waits for one that can be.
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
