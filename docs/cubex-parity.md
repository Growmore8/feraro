# Cubex-v2 parity: the whole list

The goal, stated plainly: **everything Cubex-v2 does, Feraro does.**

This file is the checklist that makes "100%" a thing we can both count
rather than a thing we argue about. It was built by reading Cubex's own
source — every page, every API route, every Prisma model — not from memory.

## What we are actually cloning

| | Cubex-v2 | Feraro today |
|---|---|---|
| Pages | 40 | 4 terminals (sign-in, client, desk, mobile) |
| HTTP/WS endpoints | 183 API routes | ~60 wire messages |
| Data models | 33 Prisma models | journal events + 3 config files |
| Lines | ~39,200 TypeScript | ~47,000 (Rust engine + gateway + web) |

Feraro is not behind on *size*. It is behind on *surface*. Cubex spends its
39k on forty screens because Prisma and Postgres hand it the data layer for
nothing; Feraro's 47k includes a matching engine, an append-only journal, a
double-entry ledger and a binary wire format that Cubex does not have and
does not need.

That is the one thing that cannot be cloned literally, and it is not a gap
to close: **a feature that is a column in Cubex is an event in Feraro.**
Where a row below says *(engine)*, that is the cost — a new event tag, a
fold, a replay test, and the old tag still decoding forever.

## The one decision that is not mine

Eleven of Cubex's forty pages are **superadmin and tenancy**: tenants,
packages, trials, billing, invoices, outsourcing, API keys, impersonation,
cross-tenant analytics. Cubex is a product sold to brokers. Feraro, as
specified so far, is one broker's own platform, and the sign-in server
picker covers several brands on several servers.

Cloning that tier is not a screen; it is a column on every account id,
journal and user store in the product. **Say the word and it goes on the
list.** Until you do, the rows below leave it out, and parity is measured
against the other twenty-nine pages.

---

## Signing in

| | Cubex | Feraro |
|---|---|---|
| Password | yes | yes, Argon2id |
| Two-factor (TOTP) | yes | **yes** |
| Register yourself | yes | — desk creates logins |
| Email verification | yes | — |
| Forgot / reset by email | yes | — desk resets |
| Google sign-in | yes | — |
| Passkeys / WebAuthn | yes | — |
| PIN unlock | yes | — |
| Sign-in audit | yes | yes, journaled |
| Impersonate a user | yes | — *(superadmin tier)* |

## Accounts

| | Cubex | Feraro |
|---|---|---|
| Sub-accounts | yes | yes |
| One demo per login, 30 days | yes | yes |
| Account requests (client asks for one) | yes | — |
| Expired-account sweep | yes | yes, demo expiry |
| Lock / unlock | yes | yes |
| **Deactivate / activate** | yes | — *(second state)* |
| **Do-not-liquidate flag** | yes | — |
| Force liquidation / wipe | yes | partly: close every position |
| Delete client | yes | **deliberately not** — ids are never reused and the journal is append-only |
| Per-account leverage | yes | — *(engine)* |
| Per-account margin-call level | yes | — *(engine)* |
| Per-account max lot | yes | — *(engine)* |
| Swap-free accounts | yes | — *(engine)* |

## Money

| | Cubex | Feraro |
|---|---|---|
| Deposit requests | yes | yes |
| Withdrawal requests | yes | yes |
| Saved payout methods | yes | yes |
| Payment methods the desk publishes | yes | yes |
| Bonus in / out | yes | yes |
| Insurance | yes | yes *(engine)* |
| Transfer between accounts | yes | yes *(engine)* |
| One approval queue for all of it | yes | yes |
| Manual P&L adjustment | yes | yes |
| Financial history, editable | yes | **no, by design** — the journal is the record |
| MoonPay / card top-up | yes | — needs a provider and a licence |
| Fund settings (min/max, fees) | yes | — |

## Trading

| | Cubex | Feraro |
|---|---|---|
| Market, limit, stop, stop-limit | yes | yes |
| Stop loss / take profit | yes | yes |
| **Partial close** | yes | **yes** *(engine)* |
| **Amend a resting order** | yes | — next up |
| Trailing stop | yes | — *(engine)* |
| Order expiry | yes | — *(engine)* |
| Hedging / netting | yes | yes |
| Manual trade by the desk | yes | yes |
| Correct a trade after the fact | yes | yes |
| Commission | yes | yes *(engine)* |
| Swap / overnight financing | yes | yes *(engine)* |
| Margin call and stop-out | yes | yes *(engine)* |
| One-click trading toggle | yes | — |
| Price alerts | yes | — |

## Pricing and instruments

| | Cubex | Feraro |
|---|---|---|
| Per-symbol spread markup | yes | yes |
| Commission per unit | yes | yes |
| **Symbol administration from the UI** | yes | **yes** — open/close and sizes |
| Bulk spread edit | yes | **yes** |
| Per-account symbol overrides | yes | — |
| Symbol permissions per account | yes | — |
| **Trade groups** (markup/swap per group) | yes | — |
| Feed failover and its log | yes | yes, aggregator + source on each quote |
| Feed test / health page | yes | health endpoint only |

## People

| | Cubex | Feraro |
|---|---|---|
| Roles | superadmin, admin, manager, client | admin, manager, client |
| **Per-manager permissions** | yes | **yes** |
| Assign clients to a manager | yes | yes |
| KYC upload | yes | — states only, no documents |
| KYC review queue | yes | states only |
| Client profile (name, phone, country) | yes | — login is the identity |
| Avatar upload | yes | — |

## Screens the desk has

| Cubex tab | Feraro |
|---|---|
| Overview | — figures are in the strip |
| Trade | yes |
| History | yes |
| Summary | **yes**, client only — desk needs one |
| Clients | — right-click only |
| Audit | yes |
| Payments | — right-click only |
| KYC | — right-click only |
| Requests | yes |
| Symbols | **yes** |
| Groups | — |
| Risk | — |
| Copy Trading | — |
| Signals | — |
| Broadcast | — |
| Referral | — |

## The client terminal

| | Cubex | Feraro |
|---|---|---|
| Market watch left, chart centre, order rail right | yes | **yes** |
| Favourites | yes | **yes** |
| Search, spread, change columns | yes | **yes** |
| Contract specification | yes | **yes** |
| Positions / Pending / History / Summary / Requests | yes | **yes** |
| Margin-level bar | yes | **yes** |
| Right-click menus | yes | **yes** |
| Wallet (pay in, withdraw, payout book) | yes | **yes** |
| Mobile | yes | yes |
| KYC banner | yes | — |
| Margin-call banner | yes | toast only |
| Notification bell | yes | — |
| Account switcher in the header | yes | left column |
| Avatar upload | yes | — |
| One-click trading | yes | — |
| Price alerts | yes | — |
| CSV export | yes | — |
| Chart indicators a trader picks | yes | MACD, fixed |
| Drawing tools | yes | — |

## Everything else

| | Cubex | Feraro |
|---|---|---|
| Notifications, in-app | yes | toasts |
| Push notifications | yes | — |
| Email (statements, confirmations) | yes | — |
| SMS | yes | — |
| Broadcast to clients | yes | — |
| Economic calendar | yes | — |
| Market news | yes | — |
| Copy trading | yes | — |
| Signals | yes | — |
| Referral programme | yes | — |
| White-label branding | yes | server picker covers brands |
| PDF / CSV export | yes | — |
| Scheduled jobs (statements, stop-out sweep) | yes | rollover only |

---

## How this gets done

In the order a broker needs it, not the order it is listed:

1. **Amend a resting order** *(engine)* — the last trading gap.
2. **Symbol permissions** — which instruments one account may trade.
3. **Per-account symbol overrides** — a different markup for one account.
   Bigger than it sounds: the markup is applied once, globally, when a
   price arrives, so per-account pricing means each account sees its own
   book. Trade groups may be the better shape for it.
4. **KYC documents** — upload, review queue, storage.
5. **Client profile and avatar** — a name to show instead of an email.
6. **Per-account risk** *(engine)* — leverage, margin-call level, max lot,
   swap-free.
7. **Trade groups** — markup and swap per group of accounts.
8. **Email** — statements, margin calls, confirmations. Then password reset
   and email verification, which need it.
9. **Notifications** — in-app bell, then push.
10. **Account states** — deactivate, do-not-liquidate, account requests.
11. **One-click trading, price alerts, CSV export, chart indicators.**
12. **Copy trading**, **signals**, **broadcast**, **referral**.
13. **Reporting read model** — project the journal into Postgres, so the
    reports Cubex gets from its tables can be written against a database
    instead of a window.
14. **Passkeys, Google sign-in, PIN** — alternatives to a password that
    already has a second factor.

Each row ends the way every row in [roadmap.md](roadmap.md) ends: tests
beside the code, suites green, clippy clean, checked against a running
gateway, pushed.
