# What Cubex-v2 has that Feraro does not

A feature inventory taken from [Cubex-v2](https://github.com/Growmore8/Cubex-v2)
— its Prisma schema, its API routes and its pages — set against Feraro as it
stands. Nothing in Cubex was changed; this was read, not touched.

It is a worklist, not a verdict. Cubex is a finished product and Feraro is
not, so most rows are "not yet" rather than "wrong".

## The one difference that is not a feature

Cubex keeps balances as `Decimal` columns and updates them in place. Feraro
journals events and derives every balance by folding them.

That is why Cubex has more features and Feraro has stronger guarantees. A
column is faster to add a field to; a journal can prove what happened and
replay to any point. Neither is better in the abstract, but it decides how
each row below gets built: in Feraro a new kind of money movement is a new
**event**, not a new column, and events are permanent.

Where a row below says *needs a journal event*, that is the cost.

## Roles and tenancy

| | Cubex | Feraro |
|---|---|---|
| Roles | superadmin · admin · manager · client | admin · manager · client |
| Tenancy | many tenants in one database | one platform per **server** |
| Tenant billing, plans, trials, invoices | yes | — |
| Impersonate a user | yes | — |

Feraro answers the same question a different way: a server *is* a tenant, and
the sign-in screen picks one. That covers brands and regions. It does not
cover one operator administering many brands from a single screen, which is
what Cubex's superadmin tier is for. Adding that to Feraro is not a UI change
— every account id, journal and user store is per-server today.

## Signing in

| | Cubex | Feraro |
|---|---|---|
| Password | yes | yes (Argon2id) |
| Two-factor (TOTP) | yes | — |
| Passkeys / WebAuthn | yes | — |
| Google sign-in | yes | — |
| PIN unlock | yes | — |
| Email verification | yes | — |
| Forgot / reset password by email | yes | — (desk resets it) |
| Sign-in audit trail | yes | journaled, shown in activity |

**TOTP is the gap that matters.** A desk login can move money, and it is
single-factor today.

## Accounts

| | Cubex | Feraro |
|---|---|---|
| Sub-accounts | yes | yes, up to 8 |
| One demo per login | — | yes, 30 days |
| Demo expiry | yes, 30 days | yes |
| Leverage per account | yes, editable | fixed at open |
| Margin-call level per account | yes | one global policy |
| Max lot size per account | yes | per instrument only |
| Swap-free (Islamic) | yes | — |
| Do-not-liquidate | yes | — |
| Locked vs deactivated | two states | one (suspended) |
| Trade groups | yes, with per-group spread and swap | — |
| Per-account spread markup | yes | — |
| Online presence, last device | yes | — |
| Bonus expiry, credit settlement window | yes | — |
| Pool account (A-book) | yes | — (`book` crate unused) |

## Money

Cubex records twelve kinds of movement; Feraro records four.

| Cubex `FinType` | Feraro |
|---|---|
| `DEPOSIT`, `WITHDRAWAL` | yes |
| `BONUS` | yes (credit, set absolutely) |
| `PNL_ADJUST` | yes (`TradeAdjusted`) |
| `CREDIT_IN`, `CREDIT_OUT` | — deltas rather than an absolute set |
| `BONUS_OUT` | — |
| `INSURANCE`, `INSURANCE_OUT` | — |
| `TRANSFER_IN`, `TRANSFER_OUT` | — **needs a journal event** carrying both legs |
| `REFERRAL` | — |

Also missing, and all three are the same underlying gap:

* **Free-text description on a movement.** Feraro's wire format is
  fixed-width — there is no variable-length string in it — so the money
  dialogs offer a chosen reason instead. Free text needs a length-prefixed
  string in the codec.
* **A reference field** (bank reference, transaction hash). Same cost.
* **Backdating** (`FinMode.MANUAL`). Cubex lets an operator set the applied
  date. Feraro must not rewrite journal time — order is the property
  everything rests on — so the right version is a *value date* recorded
  beside the real one. Also needs the string or a date field.

**Transfer between accounts is the most valuable one here**, and it should be
a single event carrying both legs so a crash cannot leave half of it.

## Trading

| | Cubex | Feraro |
|---|---|---|
| Market orders | yes | yes |
| Limit / stop | yes | yes |
| Stop-limit | yes | in the protocol, untested |
| Order expiry (day / GTC) | yes | — |
| Stop loss / take profit | yes | yes |
| **Trailing stop** | yes | — |
| **Partial close** | yes | — |
| **Modify an open trade** | yes | levels only |
| Modify a resting order | yes | — |
| **Commission per lot** | yes | — |
| Swap long/short per symbol | yes | yes |
| Swap multiplier per group | yes | — |
| Hedging vs netting | — | yes |
| **Copy trading** | master/follower with lot ratio | — |
| Trade comment | yes | — |
| Client IP on open and close | yes | — |
| Close reason | manual/TP/SL/MC/stop-out/copy | journaled per event |

## Symbols

Cubex administers symbols from the UI; Feraro's catalogue is in code.

Missing in Feraro: per-tenant symbol lists, enable/disable, digits and
category editing, **spread markup** (fixed or percentage, with a cap),
commission per lot, per-group and per-account symbol overrides, and the bulk
spread tools.

**Spread markup is how a broker makes money on a B-book.** Feraro passes the
feed's spread through untouched, which is honest and earns nothing.

## Market data

The one area where Feraro is ahead. Four feeds — Binance, Kraken, Twelve Data,
Finnhub — behind an aggregator that picks one per instrument, says which on
every quote, and fails over. Cubex logs feed failures and tests feeds, which
Feraro does not surface.

## What a client can do

| | Cubex | Feraro |
|---|---|---|
| Trade, chart, statement | yes | yes |
| Deposit by crypto / UPI / bank | yes | yes |
| **Withdrawal request** | yes, with saved methods | — |
| Upload a payment slip | yes | — |
| MoonPay card purchase | yes | — |
| **Transfer between own accounts** | yes | — |
| **Price alerts** | yes | — |
| **Notifications**, in-app and push | yes | toasts and sounds |
| Email statement | yes, and scheduled | — |
| Economic calendar, news | yes | — |
| Referral programme | yes | — |
| KYC document upload | yes | — (desk records a decision) |
| Profile, avatar, PIN | yes | — |
| Request an extra live account | yes | desk opens it |

## What the desk can do

Feraro covers: the book, positions, history, net trade, audit, deposits,
funding, suspension, hedging, demo renewal, verification, roles, manager
assignment, putting a trade right, dealing on a client's account, closing
everything.

Cubex adds: **partial close**, **modify a trade**, bulk operations on history,
reports, emailed statements, **broadcast to clients**, trade groups, symbol
permissions, spread defaults, signals, copy-relation management, account
requests, and KYC document review.

## Platform and operations

| | Cubex | Feraro |
|---|---|---|
| Scheduled jobs | cron: billing, stop-out, statements, cleanups | in-engine, continuous |
| External API for partners | `/external/v1` prices, candles, PnL | — |
| Webhooks | yes (MoonPay) | — |
| Email (SMTP) and SMS | yes | — |
| Web push | yes | — |
| API keys, usage stats | yes | — |
| Health endpoint | — | yes, 503 when prices stop |
| Backups with a tested restore | — | yes |
| Journal / replay / audit proof | — | yes |

Feraro's stop-out is continuous inside the engine rather than a once-a-minute
cron, which is the stronger arrangement: a minute is a long time in a falling
market.

## Where to start

Ordered by what a broker actually needs first, not by size.

1. **Spread markup**, per symbol with a per-group and per-account override.
   Without it the platform cannot earn anything.
2. **Commission per lot**, for the same reason, and it needs a ledger book.
3. **Withdrawals** — clients can pay in but not ask to take money out.
4. **Transfer between accounts** — one journal event, both legs.
5. **Two-factor on desk logins.** Single factor guards the money today.
6. **Partial close** and **modify an open trade** — ordinary dealing actions
   whose absence is felt daily.
7. **Trailing stop**, and **expiry on resting orders**.
8. **Per-account leverage, margin-call level and max lot** — risk settings a
   desk expects to set per client.
9. **Notifications**: email first, then push. A margin call nobody is looking
   at is not a margin call.
10. **Symbol administration** from the UI, replacing the compiled catalogue.
11. **Price alerts**, **referral**, **copy trading** — product features, once
    the above is in.
12. **Multi-tenancy**, only if one operator must run several brands from one
    screen. Server selection already covers several brands on several servers.
