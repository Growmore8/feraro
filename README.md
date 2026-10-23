# Feraro

A proprietary multi-terminal trading platform — Client, Manager and Admin over one
owned matching, risk and liquidity core. No white label.

The full build plan, stack rationale, phase sequence and effort model live in the
[Platform Blueprint](https://claude.ai/code/artifact/d70f71fe-f299-429d-ad0f-2ddd7d0003a5).

## Status

**P0 and P1 complete.** The loop is closed: the gateway aggregates live prices
from four providers into the engine, the browser places an order, the engine
checks margin and journals the fill, and the position comes back. Restarting the
gateway re-derives every balance from the journal — including positions in
instruments quoted in another currency, which replay at the rate journaled with
the fill rather than today's.

Verified end to end against live prices: an affordable order fills at the ask,
repeated 9 BTC orders fill until free margin runs out and the next is refused
with `insufficientMargin`, an off-step size is refused with `invalidQuantity`,
and every position closes even at a 119% margin level. After a restart the
balance came back as `25597.965450000` — the same nine decimal places it went
in with.

| Crate | What it holds | Tests |
|---|---|---|
| `money` | Fixed-point `Price`, `Qty`, `Money`; P&L; conversion | 18 |
| `domain` | Instruments, accounts, the `Event` vocabulary, wire codec | 11 |
| `journal` | Segmented append-only log, torn-tail repair, streaming replay | 25 |
| `ledger` | Double-entry, zero-sum invariant, cash vs. bonus | 13 |
| `book` | Price-time priority order book and matching | 17 |
| `risk` | Margin, admission, calls, stop-out, swap, conversion, S/L, hedging | 63 |
| `pending` | Resting orders and the price conditions that fire them | 17 |
| `rollover` | Trading-day calendar and overnight financing | 14 |
| `web/lib` | Terminal money and indicators — parity with the engine | 36 |
| `web/feed` | Exchange and gateway payload parsers | 57 |
| `web/lib` (layout, ticket) | Remembered panes, the ticket's price step | 19 |
| `gateway` | Engine, feeds, accounts, the desk, books, bars, snapshots, roles, payments | 164 |

## Layout

```
engine/          Rust workspace — the trade path. Zero third-party dependencies.
  crates/
    money/       Fixed-point arithmetic. No f64 reaches a balance.
    domain/      Event vocabulary and its versioned wire format.
    journal/     The event log. This, not PostgreSQL, is the source of truth.
    ledger/      Double-entry books, including non-withdrawable bonus.
    book/        Matching.
    risk/        Account state, margin, admission, stop-out.
    pending/     Limit and stop orders, and what activates them.
    rollover/    The daily calendar and swap arithmetic.
web/             React + TypeScript terminals. The client terminal and the
  src/lib/       manager terminal share one component library.
    money.ts     The engine's fixed-point rules, in TypeScript. See below.
    domain.ts    The engine's enums, plus trader-facing rejection copy.
    indicators.ts  MACD and RSI. The one place floats are allowed, and why.
    instruments.ts The listed symbols.
    mock.ts      Account and positions only — no prices.
  src/feed/      Live market data. See below.
  src/components/  Panel, DataTable, Pill, Button, Field, PriceChart, Toasts,
                   MarketWatch and ChartPane — the two panes both terminals show.
  src/terminals/   SignIn, ClientTerminal, ManagerTerminal.
gateway/         WebSocket edge. Owns the engine, the price feeds and the
  src/           browser protocol. This is where the dependencies live.
    engine.rs    The single-writer task. Nothing else touches the portfolio.
    auth.rs      Logins, roles, password hashes and session tokens.
    candles.rs   Bars, built from the prices the gateway has seen.
    snapshot.rs  Engine state on disk, so startup stops reading all history.
    feeds/       Binance, Kraken, Twelve Data, Finnhub, and the aggregator
                 that picks one per instrument. Keys stay server-side.
    instruments.rs  The catalogue: terms, swap, and each symbol's feed order.
    protocol.rs  The wire format. Every money value crosses as a string.
design/          The design canvas source — .dc.html artboards per screen.
docs/adr/        Architecture decisions, with the reasoning that produced them.
```

## Why the money rules exist twice

`web/src/lib/money.ts` is a deliberate reimplementation of `engine/crates/money`,
not a binding. Both sides use scaled integers — `bigint` in the browser, `i64`
and `i128` in the engine — and both round half away from zero.

They are kept honest by asserting the same values from the same inputs:
`money.test.ts` and the crate's `mod tests` share their cases. Change the
rounding rule on one side and the other side's suite fails. A browser and an
engine that disagree about a P&L figure produce a support ticket nobody can
close, so the duplication is cheaper than the alternative.

## The journal on disk

The journal is a directory of numbered segments, not one file:

```
journal/
  feraro.000001.jnl
  feraro.000002.jnl
  feraro.000003.jnl   ← active
```

A segment is sealed and a new one opened when it passes
`FERARO_SEGMENT_BYTES` (8 MB by default). Sequence numbers run unbroken across
files, so a gap at a boundary is as loud an error as a gap inside one, and
replay streams one segment at a time — startup memory is bounded by the segment
size however long the journal has been running.

**Only the last segment may be torn.** A process killed mid-append leaves a
short final frame in the file it was writing, and that is repaired. A *sealed*
segment was closed cleanly and can never legitimately be short, so a torn tail
in segment 3 of 7 is not a crash to recover from — it is corruption, and replay
refuses it rather than silently serving a portfolio missing four days of
trades. A missing segment number is refused for the same reason.

## The two terminals

**The client terminal** is the trading screen: chart in the middle, market
watch on the right, blotter across the bottom, deal ticket over the top of it.
Desktop and phone — see *On a phone* under Market data.

**The manager terminal** is the desk's, and it is deliberately the same shape:

```
 +----------+------------------------+--------------+
 | clients  |         chart          | market watch |
 |   Demo   |                        |              |
 |   Live   +------------------------+--------------+
 |          |  toolbox: accounts . positions .      |
 |          |  people . books . audit . activity    |
 +----------+---------------------------------------+
```

Clients on the left, because that is what a manager navigates by: grouped
**Demo** and **Live** under each client's email address, with how many days
each demo has left. The chart in the middle, because it is the thing you look
*at*. Prices on the right, where the client terminal keeps them, so what the
desk learns on one screen transfers to the other. The toolbox along the
bottom, because its tables are wide and shallow.

**Clicking an account in the tree selects it**, and the toolbox then shows
*their* positions and *their* figures. It is the same `SelectAccount` a client
uses on their own account picker, and the same entitlement check decides it —
so every action the desk takes is journaled with the operator's name on it
(invariant #9: no account is acted on without an entitled session).

**The chart and the market watch are one component each**, shared by both
terminals. Two implementations of a price table drift apart, and a drift in a
price is the one kind that matters.

### Signing in

A split screen: what this is on the left, the form on the right. A back
office is opened by people who were sent a link and need to know they are in
the right place, so the left half says so — and it is the only large field of
colour in the product, because once you are inside, the screen is data and
the palette gets out of the way. Below 900px the left half goes.

There is deliberately **no "keep me signed in"**. A token that outlives the
tab is a token somebody else can use on a shared desk machine.

### The navigator is a tree

```
 Feraro
 ├ Favourites              pinned accounts, remembered per browser
 ├ Accounts
 │ ├ boss@feraro.local     the folder IS the manager
 │ │ ├ Live   1004
 │ │ └ Demo   1001  1002
 │ └ Unassigned
 │   └ Demo   1003
 └ Managers                who exists, and how many each holds
 ─────────────────────────
 [ Navigator ][ Data Window ]
```

Grouping by manager is the arrangement the desk actually has. Accounts whose
client nobody holds sit under **Unassigned**, named rather than hidden: an
account with no manager is a fact somebody should see, not an absence. Live
and demo are split inside each folder because one of them holds somebody's
money.

**Tabs sit at the foot of the pane**, in both the navigator and the toolbox.
The contents are the subject; the tabs are how you change it, so they read
last. The navigator's second tab is a **data window** — the symbol's prices
and the selected account's figures together, because on a desk the question
is rarely about one without the other.

### The blotter adds up

The positions table carries a totals line: how many are open, the financing
accrued, and the running profit. A blotter whose figures do not add up to
anything is a blotter somebody adds up by hand. Price and financing are kept
apart as well as together — one moves with the market, the other was charged.

Above the toolbox sits the **balance summary**: balance, equity, margin, free
margin, margin level and profit for the selected account, amber because it
qualifies everything around it and reading it as the house's figures would be
the expensive mistake.

### Right-click is the verb

Every action on this screen has one subject: an account, a position, a
symbol. A strip of buttons has to *state* its subject — "deposit to 1001" —
and stay on screen whether or not anybody wants it. A right-click already
carries its subject: the row under the pointer. So the buttons are gone and
the menus are specific.

| Right-click | What it offers |
|---|---|
| An account, in the navigator or the Accounts tab | act on it, deposit, withdraw, set bonus, suspend, reinstate, hedging, renew, make live, put a trade right, show its activity |
| An open position | close it, set a stop loss, set a take profit, chart the symbol |
| A price in the market watch | chart it, buy, sell |
| The background | **new client**, **new manager**, refresh, and the View tick-boxes |

Creating a person lives on the background because a new client is not an
action *on* an existing one. A manager is created in one gesture rather than
created-then-promoted: two steps to make one thing is two chances to leave it
half done.

Anything needing a figure opens a small dialog rather than `window.prompt` —
it names the account, says what is about to happen, and focuses the field.
An amount typed against the wrong account is the mistake this screen exists
to prevent, so the subject is on screen while the figure is typed.

The menu closes on the next click, on Escape, on a scroll and on a resize. A
menu floating over ticking prices goes stale, and a stale menu that still
acts is how the wrong account gets funded.

### The sounds

Nine alerts, **synthesised rather than shipped as files**. That is not
frugality: it means each one is specified rather than chosen, and the
relationship between them lives in the code where it can be argued about.

| | pitch | shape |
|---|---|---|
| sign-in · sign-out | rising · falling | two soft notes |
| opened · closed | mid | one short blip |
| take profit · stop loss | rising third · falling third | bright · muted |
| margin call | high | three urgent beeps |
| liquidated | low | long, and deliberately dissonant |
| refused | low | a flat buzz |

Pitch falls as the news worsens, and only the two that demand action repeat.
A trader learns the set in an afternoon without being taught it.

Sound is off by a tick box in the **View** menu and remembered per browser.
Nothing plays until the page has been interacted with, because browsers refuse
audio until then and are right to.

Sign-in and sign-out are the one pair with no push behind them: a session
change is journaled and read from the activity feed, which the desk polls. It
is as live as the record gets without a new wire message, and it is honest
about being a poll.

### Arranging the screen

All four panes — navigator, chart, market watch, toolbox — **resize and
close**. Drag the edge between any two; double-click that edge to hide the
pane; or use the **View** menu in the command bar, which has a tick-box for
each and a reset. Closing the last open pane is refused, because an empty
screen is not a layout.

The arrangement is remembered in `localStorage`: a desk sets its screen up
once and expects to find it that way tomorrow. It is *this browser's*
preference, not the account's — signing in on another machine should not drag
somebody else's column widths along, and none of it is worth a round trip to
the gateway. Every read is defensive, because storage can be unavailable,
cleared, or hold something an older version wrote; in each case the answer is
the default layout rather than a broken screen.

**One screen, no second terminal.** The desk deals from a ticket over the
chart, on the account the tree has picked out, and manages what it opened from
the Positions tab below — closing, and setting a level, where the position is
already listed. A button that sends an operator to another screen to do half
the job is a button that loses them the price.

### Manager and administrator are the same screen

A manager sees exactly what an administrator sees. The difference is the book
behind it.

| | Administrator | Manager |
|---|---|---|
| Accounts in the navigator | every one | their assigned clients' |
| Deal, close, set a level | yes | yes, on their own clients |
| Suspend, reinstate, renew a demo, hedging | yes | yes, on their own clients |
| Deposit, withdraw, bonus | yes | no |
| Make an account live | yes | no |
| Approve identity | yes | no |
| Create a login, set a password, set a role, assign a client | yes | no |
| The books and the audit trail | yes | no |

The split is money and identity on one side, working the book on the other. A
manager runs their clients; an administrator makes the arrangement that gave
them those clients, and stays accountable for the money in them.

**An administrator assigns clients to a manager** from the People tab, and
sees in the navigator and the Accounts table whose book each client sits in —
or that nobody has them. Promoting a client to manager, and demoting one, are
in the same place. Demoting leaves their clients unassigned rather than in the
book of somebody who can no longer work it: an assignment that points at a
login without the role reads as supervision that is not happening, which is
worse than none.

**The limit is on whose accounts, not on which buttons.** A manager's session
carries the list of accounts it may act on, exactly as a client's does, and
`Signed::may_act_on` checks against it. Hiding the actions a manager may not
take is a courtesy so they are not offered a button that would fail;
`desk_refusal` in the gateway is the rule, and it refuses a manager who names
an account outside their book by number.

**Who manages whom is not in the journal.** It is a relationship between two
people, like the password and the identity check, and it gets reassigned —
which an append-only log is the wrong shape for. What the journal keeps is the
consequence: every action the manager took, with their name on the event. The
audit trail reads `boss@feraro.local — suspended on 1001` whoever reassigns
them afterwards.

**There is still no dealer role.** Three roles, `client`, `manager` and
`admin`, ordered from least to most so a check reads as "at least this role".

## Accounts and signing in

**The login is an email address**, and only one account may have it. Two
people sharing a login is two people sharing an account, so a duplicate is
refused — case-insensitively, because nobody remembers whether they
capitalised it — and refused in those words rather than as a generic error.

**Every account belongs to a person.** An account is opened the first time its
owner signs in, and `Event::AccountOwnerSet` records the link in the journal
beside the trades — replaying reproduces not just the balances but whose they
are. An account with no owner is one nobody can be held to, which is why the
engine no longer invents one at startup.

**One demo account per login, and only one at a time.** A practice account is
a trial, and a trial somebody can take again the moment it ends — or hold
five of at once — is not a trial, it is an unlimited free product with an
expiry date on each copy. A second demo is refused while one is running. An
expired one does not block a new one: the thirty days running out is the end
of that trial, and whether another may start is the desk's decision, made by
renewing the old one or letting them open a fresh one.

**Live accounts are not limited that way** — up to `ACCOUNTS_PER_USER`, eight.
They are sub-accounts of a funded relationship, used to keep strategies apart,
and the money in them is already the client's. Each is a first-class account
with its own number, balance and positions; what they share is the email that
owns them, which is how the manager's tree groups them.

### Demo, live, and the thirty days

**Every account opens as demo.** An account that nobody deliberately made live
should never be live. The distinction is journaled (`AccountClassified`), not
configured — it decides whether the money in the account is somebody's, and
that is not a question to answer from a settings file.

**A demo account expires thirty days after it opens.** A practice account that
never expires quietly becomes a permanent one: stale positions, a forgotten
balance, and eventually somebody treating the figure as real. Past its thirty
days the account is refused *opening* orders, with `demoExpired` — a rejection
reason of its own, because the remedy is not a suspension's and the trader
should be told which they are looking at. It can still **close** what it
holds: an expiry is a clock running out, not a sanction, and locking somebody
into a position is not what it is for.

The clock reads the journal's timestamps, not the wall clock, so a replay
dates an account exactly as it was dated the first time. `AccountRenewed`
starts it again — thirty more days, with the operator's name on the event.
Renewing a live account is refused: it never expired.

### Paying money in

**There is no payment provider, and the platform does not pretend otherwise.**
What it has is the instructions and a queue.

An administrator publishes where money is sent — **crypto**, **UPI** or **bank
transfer**, each with an address, a name the client will recognise, and
anything they must do to be credited. Clients see the ones switched on, copy
the address, pay, and then tell the platform they have paid.

That claim credits nothing. It goes into a queue the desk works: somebody
looks at the wallet or the bank statement, sees the money, and confirms it —
and *that* is an ordinary journaled `FundsDeposited` with the operator's name
on it, the same event as any other deposit. There is one record of money
moving and one answer to who authorised it.

The ordering is the point. A platform that credits an account because a client
typed a reference number is a platform that pays out money it never received.

A few things follow from that, and each is enforced rather than hoped for:

* A claim leaves `pending` exactly once, so a double click cannot pay twice.
* One open claim per account per method — pressing the button again is not
  paying again.
* A method that is switched off cannot be paid to, and a client cannot raise
  a claim against one.
* Retiring a method **hides** it rather than deleting it. A client may already
  be paying to that wallet, and the desk has to recognise the address when the
  money lands.
* If the engine refuses the credit after the claim is settled, the operator is
  told the account was **not** credited and to deposit by hand — rather than
  being shown a success nobody received.

Methods and claims live in `payments.json` beside the logins, not in the
journal: a wallet is rotated and a bank changes, and an append-only log is the
wrong shape for a thing whose current value is the only one that matters. What
*is* journaled is the money.

### Identity checks

**A live account needs the client verified.** `Kyc` is `none`, `submitted`,
`approved` or `rejected`. The client asks from their own terminal; only the
desk decides; and until it is `approved`, asking for a live account comes back
refused, saying why. The gate sits in the session layer, which holds the
logins — the engine knows nothing about identity and should not have to.

Identity lives with the logins rather than in the journal, for the same reason
the password hash does: documents expire, details change, and a person may ask
to be forgotten — none of which an append-only log can honour. What *is*
journaled is the consequence: the account becoming live, with the
administrator who approved it named on the event.

**Document collection and checking are not built.** Approving records the
desk's decision; it verifies nothing. That is where this actually stands, and
it is the gap to close before the word "live" means what it implies.

**Credentials are not in the journal.** The journal is append-only and
permanent; a password hash has to be rotatable, resettable and erasable. So
hashes live in `users.json` (Argon2id, one PHC string per user, created next to
the journal and git-ignored) and only the user-to-account link is journaled.
User ids are never reused, so an account cannot be re-pointed at a new person
by deleting and re-adding a login.

**Two roles: client and admin.** A client trades the accounts they hold and
sees nothing else. An admin runs the platform and may act on any account — but
a session always *starts* on the signer's own account, and acting on a client's
is a deliberate switch. An admin who signed in and landed on the first client
in the book would be one mis-click from trading someone else's money.

**The gateway answers nothing before a session exists.** A socket is told
`authRequired` and ignored until it signs in — not even prices go out. Then
`login` returns a token to resume with, held in `sessionStorage` so a refresh
does not ask again and closing the tab ends it.

**Every broadcast is addressed.** Prices go to everyone; a fill, a portfolio, a
margin call, a stop-out and a rollover go to the one account they concern, and
a session is sent only what the account it is watching is entitled to. With two
traders signed in, the alternative is one of them reading the other's
positions.

**Authorisation is checked twice.** The session decides which account a command
carries — a frame has nowhere to name one — and the engine then checks that the
ticket named by a close or a cancel is actually on that account. Either check
alone would be enough today; both means a bug in one is not a breach.

**What refusals do not say.** A wrong login and a wrong password are the same
answer, and an unknown login still pays for a password verification so the two
take the same time. Asking after an account that is not yours gets the same
answer whether or not it exists. Five wrong attempts make a login wait a
minute, counted per login rather than per connection — a new socket is free.

**Not yet:** single factor only, no sign-in audit trail, and sessions are held
in memory, so a gateway restart signs everybody out. That last one is the safe
direction to fail, and a token that survives a restart needs revocation that
survives one too.

## Administration

The manager terminal is the desk: the book, the people on it, and what was
done to them. There is no separate dealer role — an administrator does desk
work themselves.

**Six tabs in the toolbox, in the order an operator asks questions.**
*Accounts* lists every account with balance, credit, equity, margin in use,
margin level, open positions and whether it is suspended, with the five house
figures across the top. *Positions* shows the open trades on the account the
tree has selected. *People* lists logins, which accounts each holds, each
client's verification state, and creates a client or resets a password.
*Books* is the trial balance. *Audit* shows what was done and by whom.
*Activity* is one account's own history.

**The action strip acts on the selected account**, and nothing more: deposit,
withdraw, bonus, suspend and reinstate, make live, renew for thirty days,
switch between netting and hedging — and an amend strip for putting a trade
right. Selecting an account in the tree is what arms them, so there is no
screen on which an action has no stated subject.

**Money is journaled with the operator's name on it.** `FundsDeposited`,
`FundsWithdrawn` and `CreditSet` each carry a `by` user id, so the question
"who authorised this" is answered by the same record that moved the money
rather than by a log beside it. `OrderAccepted` and `PendingOrderPlaced` carry
one too: an order an administrator placed on a client's account and one the
client placed themselves are the same trade with different answers to who
decided it, and that is the answer a dispute turns on.

**Credit and suspension are events, not flags.** They used to be set straight
onto the account at startup, which meant a bonus that worked until the next
restart and then quietly did not. Both are now folded from the journal like
every other piece of state — that is invariant P0-2, and it had a hole in it.

**A withdrawal cannot touch margin.** It is checked against *free* margin, not
the balance: paying out money that is holding a position open would stop the
account out on its way to the bank. **A bonus is set, not added** — an operator
decides what the bonus *is*, and a delta cannot be read later without knowing
what it was applied to.

**Suspension refuses new orders and nothing else.** Open positions stay open
and the engine still stops the account out if it has to: a sanction is not a
reason to leave a loss unprotected, and trapping a trader in a position is not
a sanction anyone intends.

**The audit view is a window, not the record.** The journal holds every
attributed event for ever; the engine keeps the most recent few hundred in
memory, rebuilt by replay, so a screen can be served without reading the whole
history. Prices, rollovers and stop-outs are not in it — they are the engine's
own doing, and an audit answers who *decided* something.

**Role checks happen in one place.** The session refuses an administrator's
request from a client before it reaches the engine; the engine records who
acted but does not decide who may. Logins never reach the engine at all — it
holds user ids, and the session joins the names on.

### Putting a trade right

Sometimes a fill is genuinely wrong: a stale quote, a crossed book, a dealing
error. The platform can correct that, and the way it does is the whole point.

**Nothing edits a filled trade.** The journal is append-only, so an amendment
is a *new* event beside the original, carrying the amount or the price, the
reason, the ticket it concerns and the administrator who made it. Two kinds:

- **An adjustment** moves money onto or off the account, for a named reason —
  `badPrice`, `dealingError`, `feeReturned`, `goodwill` or `other`. The reason
  is a fixed list, journaled: "the operator typed something" is not an answer
  anybody can audit, and a reason that cannot be counted cannot be reported on.
- **A price correction** changes what an *open* position is treated as having
  opened at, which changes what it will realise from here. It moves no money,
  because nothing has been realised. A closed trade cannot be corrected — it is
  already in the balance, and putting that right is an adjustment, which says
  in money what was wrong.

**The client is told**, in the same moment, on their own terminal — not only
the administrator who did it. A correction nobody tells the client about is a
correction nobody can question.

**Why it is built this way.** Editing a filled trade in place is the single
most abusable thing a B-book can do: a losing trade quietly becoming a winning
one, a stop-out un-happening, a client's own history disagreeing with what they
remember. No amount of policy makes a mutable trade history auditable. An
adjustment that is visible, attributed and permanent can be questioned; a
rewritten fill cannot even be noticed.

**Adjustments have their own book.** The house leg lands in
`AccountKind::Adjustment`, never in realised P&L: a broker whose corrections
are mixed into its trading revenue cannot answer the one question that matters
about them — how much, how often, and to whom.

**A debit cannot overdraw.** Taking money off is checked against the account's
cash, so an adjustment can return a fee but cannot invent a loss the client
never agreed to.

### Client activity

Who signed in, when, from where — and who failed to. `SessionChanged` is
journaled beside the trades rather than in a log of its own, because the
questions asked about a sign-in are the questions asked about a trade: who,
when, and *in what order relative to everything else*. Two writers' timestamps
do not order reliably; a journal sequence does.

**Refusals are the interesting ones.** A run of them from one address is what a
password attack looks like. A refused attempt names no user, because it proved
nothing about who it was.

**It is deliberately small:** a user, an outcome, and sixteen bytes of address.
No tokens, no password material — nothing that could sign anybody in if the
journal were read by someone who should not have it. The address is personal
data and is kept because a trading platform is asked to know it: a sign-in from
a new country is how account takeover is noticed. It is never shown to other
clients.

**It waits for the next sync**, like a quote. A sign-in is a record nobody is
waiting on, and an audit that can block a login is an audit that gets switched
off.

## The books

The `ledger` crate was written early and used by nothing. It is now folded
from the same journal as the portfolio, so the books and the balances are two
views of one history rather than two systems to reconcile.

**Every money movement is a balanced transaction**, keyed by the journal
sequence that produced it — unique, never reused, and already in the order
things happened:

| What happened | The legs |
|---|---|
| Deposit | client cash **+**, house payments clearing **−** |
| Withdrawal | client cash **−**, house payments clearing **+** |
| Bonus set | client bonus **±** the *difference*, house bonus the reverse |
| Position closed | client cash **+** profit and financing; house trading book **−** the profit, house financing book **−** the financing |

**The split between trading and financing is deliberate.** They are different
revenue to a broker and different costs to a trader, and one of them — swap —
is the number a client disputes most often. A close posts three legs rather
than two so each book answers for itself.

**Two invariants are asserted on every event, not just in the ledger's own
tests.** Client cash must equal the account's balance, and every currency must
net to zero. The gateway's test suite checks both after deposits, withdrawals,
bonuses, ordinary closes, stop-outs, protection closes and a full replay. When
the two disagree the bug is ours, and we know within one event rather than in
an audit.

**A bonus can no longer be withdrawn.** This was a real hole: a withdrawal was
checked against *free margin*, which counts the bonus because a bonus supports
margin. It is now checked against free margin **and** `ledger::withdrawable`,
which is cash only. On a 25,000 account with a 1,000 bonus, the live test asked
for 26,399.99 — inside free margin — and was refused with "the rest is
non-withdrawable bonus". A broker that pays out its own promotion does it
exactly once per client.

**The administrator's Books tab** shows every pot — client cash, bonus, the
house's trading, financing and payments books — and the trial balance per
currency, which must read *balanced*. It is the invariant made visible: an
operator seeing a non-zero residual is seeing it before an auditor does.

**Not yet:** payment-provider integration, so a "deposit" is an operator
asserting that money arrived rather than a cleared payment; commission and
introducing-broker books exist in the ledger but nothing posts to them; and
`PspClearing` is one pot rather than one per provider.

## Currency conversion

An instrument's profit arises in its **quote** currency: a USDJPY position makes
and loses yen. An account holds one currency. So every amount crossing from an
instrument to an account — profit, margin, swap — is converted, and nothing else
may reach a balance. That is invariant P1-3, and `risk`'s test suite asserts it.

**Each currency routes through one pair.** `gateway/src/instruments.rs` maps yen
to USDJPY, Canadian dollars to USDCAD, francs to USDCHF — the pair the trader is
trading *is* the pair that values their position, so there is nothing extra to
subscribe to. USD-quoted instruments convert at one, which is not a special case
but the correct rate.

**Conversion is at the mid**, and nothing is charged for it. Positions are valued
at the side they would close on; a conversion is not a trade the house takes the
other side of, so taking half a spread for turning yen into dollars would be a
fee — and a fee belongs in the ledger where it can be seen, not buried in a rate.

**The rate divides, it does not multiply by a reciprocal.** USDJPY at `147.5` is
exact in nine places; `1 ÷ 147.5 = 0.006779661…` is not. Dividing by the rate the
market quoted keeps the arithmetic exact to the last place —
`Money::convert_per`, mirrored in the browser as `convertPer`.

**The rate used is journaled with the fill.** Profit is *recomputed* on replay
from the lots and the fill price, never stored, so a yen P&L also depends on
USDJPY at the moment of the close — and that is not reproducible later, because
quotes are journaled on a throttle and the rate the engine used may never have
been written down. `Event::OrderFilled` and `Event::PositionStoppedOut` therefore
carry a `rate`, and a restart reproduces the balance to the last place however
far the rate has moved since. Swap is converted when it is charged, so the amount
in `SwapCharged` is already in the account's currency.

The two events took **new wire tags** (`0x46`, `0x47`) rather than growing a
field. Tags `0x33` and `0x41` still decode — as the same decision with a rate of
one, which is exactly what it was when every tradeable instrument was quoted in
dollars. They are never written again.

**No rate, no trade.** An instrument whose currency has no route, or whose
conversion pair has no price, is refused with `noConversionRate` — distinct from
`marketClosed`, because the instrument is quoted and tradeable and nothing is
wrong with the order. Yen counted as dollars is a 150× error in someone's favour,
so refusing is the only safe answer. For the same reason a night's financing is
charged in full or not at all: if any position cannot be converted, the whole
rollover waits for the next flush rather than letting that position off a night
or charging the rest of the book twice on the retry.

**A stop-out ranks losses in the account's currency.** Ten thousand yen is not a
bigger loss than a hundred dollars, and closing the wrong position first costs
the trader a spread for nothing.

**Still single-hop.** A euro-quoted instrument in a sterling account would need
two pairs multiplied together; each currency here routes through exactly one. An
account in a currency the market does not settle in is refused rather than
valued — `CurrencyMismatch` — which is what multi-currency accounts (P2) will
generalise.

## Snapshots

Segmentation bounds the *memory* a replay needs; it does nothing about the
*time*. A journal that has run for a year was folded from genesis every time
the gateway started. A snapshot says "this was the state at sequence N", and
startup folds only the tail after it.

**A snapshot is never the record.** It is derived from the journal, and
deleting every one of them leaves the gateway coming up with exactly the same
state, only slower. That is the property worth keeping, and it is why an
unreadable snapshot is a log line rather than a failure to start.

**The test that makes it trustworthy** is the only one that matters:
*snapshot plus tail reaches the state a full replay reaches*. It is asserted
over a journal that does everything the engine can do — deposits,
withdrawals, bonuses, a suspension and a reinstatement, a closed trade, a
stop loss, an open position with protection and financing on it, an
adjustment, a corrected entry price, a resting order placed and cancelled,
and a sign-in — by fingerprinting every balance, every parcel, every
protection level and every ledger pot, then comparing.

**Written after the sync, never before.** A snapshot of state whose events are
not yet on disk would, after a crash, describe a history the journal does not
contain. And it is written to a temporary file and renamed over the old one: a
half-written snapshot that looked complete is the one thing worse than none.

**JSON, versioned, money as decimal strings** — the same discipline as the
browser protocol, for the same reason. The journal's own wire format is
deliberately *not* reused: that format must stay byte-stable for ever because
a regulator may replay a 2029 trading day, while a snapshot is a cache, and
holding a cache to that promise would mean never being able to change it. A
snapshot from another version, or from a book that settles in another
currency, is ignored rather than guessed at.

**Bars are not snapshotted.** The gateway's own chart history is rebuilt by
re-reading the journal's quotes — which it does even for the part a snapshot
covers, because a chart with no history is worse than a slightly slower start.
Conversion routes are not snapshotted either: they are configuration the
gateway sets, and reading them from a file would let a stale snapshot quietly
change how money is converted.

`FERARO_SNAPSHOT_EVERY` sets the cadence in events (ten thousand by default);
zero turns snapshots off. The live test: a gateway restarted after 1,132
events resumed from a snapshot taken through 1,013 and replayed 119.

## Durability

**Nobody is told about a decision before it is on disk.** Engine handlers never
broadcast directly; they queue. After each command the engine syncs the journal
if anything durable was written, and only then releases the queue. A trader
told "filled" cannot find, after a crash, that the engine has no record of the
trade — the confirmation is structurally unable to leave before the fsync.

What counts as a decision is `Event::must_be_durable()`: every event except a
quote. It is an exhaustive match on purpose, so adding a variant forces someone
to decide which side it falls on.

Quotes sync on a bounded cadence instead — `FERARO_SYNC_MS`, 200 ms by default —
because an fsync per tick would cap the engine at the disk's write rate, and
losing the last few quotes changes no balance: every decision that used a price
journaled that price itself. The interval is what keeps a quiet market from
leaving quotes unsynced indefinitely.

A market order journals `OrderAccepted` and `OrderFilled`, and both ride one
sync. If an fsync ever fails, the engine stops rather than keep confirming
trades it can no longer promise will survive a restart.

## Overnight financing

Every open position is charged swap once per trading day, at
`FERARO_ROLLOVER_HOUR` UTC (22 by default — 17:00 New York, the FX day end).

**Rates are annual basis points of notional**, separately for longs and shorts:
`-1000` means a long pays ten per cent a year, and one night is
`notional × rate ÷ 10,000 ÷ 365`. MetaTrader uses points per lot instead, which
works for FX and breaks for crypto, where a point of BTC and a point of DOGE
differ by six orders of magnitude.

**FX and crypto differ, by configuration only.** FX triples on Wednesday and
charges nothing at the weekend — seven nights a week either way, the weekend
just moves. Crypto charges one night, every night.

**Swap accrues on the position, not the balance.** It shows in the Swap column,
moves equity immediately, and is realised into the balance when the position
closes — in proportion if it closes in part. The proportional split rounds, and
the remainder stays on the open part, so seven partial closes of one position
realise exactly what was charged with no residue.

**A night is charged once, however many times the engine restarts.** Each
rollover ends with a `RolloverCompleted` marker, and only days after the last
marker are ever charged. A gateway down over a weekend catches up each missed
night in order; one down for more than fourteen days pauses financing and says
so, because posting a month of charges at today's price is an operator's call.

## Stop loss and take profit

Protection belongs to a **parcel**, not to an instrument. Two lots of the same
symbol opened at different prices are protected at different prices, and
collapsing them onto the symbol would move one position's stop because another
was opened.

| Position | Closes on | Stop loss fires | Take profit fires |
|---|---|---|---|
| Long | the bid | `bid <= stop` | `bid >= target` |
| Short | the ask | `ask >= stop` | `ask <= target` |

**Measured at the price the position would execute at**, never the mid. A long
is closed by selling into the bid, so the bid is what its stop watches; using
the mid would fire a stop on a price the market never offered.

**A stop can slip.** It closes at the price the market is showing, which in a
gap is past the level the trader set — the live test closed a stop set at
81,932.78 at 81,932.31. That is what a stop is, and promising otherwise would
mean owing the difference every time the market gaps. A take profit fills at
the market too, so the trader keeps any improvement over their level.

**A gap through both levels fires the stop loss.** The alternative is paying
out the better side of a move that also blew through the worse one, on a price
the account never traded.

**Protection on the wrong side of the market is refused**, not accepted and
fired a tick later: a stop *above* a long is an order to sell at a loss
immediately, which nobody means. Levels must also sit on the instrument's tick
grid, like any other price.

**Set, moved and removed by one message.** `setProtection` says what the
protection *is* — both levels, every time — so a terminal cannot half-apply it
by forgetting a field. An empty level means none.

**Honoured before the risk sweep.** On each tick the engine fires protection
first and only then asks whether the account needs protecting from itself: the
trader's own instruction comes before the house's.

**Journaled separately from a stop-out.** `ProtectionTriggered` is the engine
doing what the trader asked; `PositionStoppedOut` is the engine protecting
itself from an account that can no longer pay. A trader reading their history
is owed the difference, and the terminal says which one closed the position.

### A stop-out now names the parcel it closed

A stop-out picks the *largest loser*, which need not be the oldest parcel —
but the journaled event carried only the size, so replay closed the front of
the queue instead and could reach a different book from the live one. The event
now carries the ticket. Journals written before it do not, and those replay
oldest-first, which is what they did at the time.

## Netting and hedging

Every account is one or the other, and the mode is journaled because it
changes what an order *means*: the same sell, on the same account, either
closes a long or opens a short beside it. Replay has to know which.

**Netting** is the default and what most brokers offer: an opposing order
reduces or closes what is open, so an account holds at most one position per
instrument, and a close is just an opposing order.

**Hedging** lets both sides be held at once, as MetaTrader 5 does. A position
is then closed by naming its **ticket** — `PositionClosed` — because an
opposing order would open a third position rather than close anything.

**The mode cannot change while anything is open.** A trader is entitled to the
rules they opened under, and switching halfway through would change what
closing their position does.

### What a hedge ties up

A long and a short of the same size in the same instrument cannot both lose:
whatever the market does, one gains what the other gives up. So the pair ties
up the **difference** between the two sides, plus `hedged_bp` of the matched
part — 2,500 basis points of both legs by default, which is a quarter of the
pair, or half of one leg.

Not zero, because a hedge is not free: both legs still cross a spread to
close, financing runs on both, and the instrument can gap out from under the
pair while the account has nothing behind it. Brokers that charge nothing here
are the ones that find out on a Monday morning. `hedged_bp: 0` charges nothing
and `10_000` charges it as though it were not hedged at all — both are
policies, and the default is neither extreme.

**Per instrument, never across them.** Long EURUSD against short GBPUSD is
*correlated*, not hedged, and charging it as hedged is how a broker discovers
the difference on the one morning the correlation breaks.

The live test: one 0.01 BTC position tied up 8.17; hedging it took the
account's margin to 4.08, not to zero and not to 16.34.

## Resting orders

Limit and stop orders rest on the `pending` book and activate against the
price they would *execute* at, never the mid:

| Order | Sits | Activates when | Fills at |
|---|---|---|---|
| Buy limit | below the market | `ask <= limit` | the ask |
| Sell limit | above the market | `bid >= limit` | the bid |
| Buy stop | above the market | `ask >= stop` | the ask |
| Sell stop | below the market | `bid <= stop` | the bid |

A limit fills at the ask rather than at its own price, so the client keeps any
improvement. A stop fills at the market and can slip past its trigger — that is
what a stop is, and promising otherwise would mean owing the difference in a
gap.

**A stop-limit rests at the limit the terminal asked for.** Its stop is where it
activates; its limit is where it then rests, which is its slippage tolerance. A
limit on the wrong side of the stop could never fill and is refused; an omitted
limit means "at the stop", which is no tolerance at all — the safe reading of
silence.

**Margin is not reserved while an order rests.** It is checked when the order
activates, which is what MetaTrader does. Reserving margin for a stop parked
forty per cent away would lock up an account for an order that may never fire.
The consequence is that a triggered order can be refused, and that refusal is a
journaled decision like any other.

## The account statement

Every movement on an account, with the balance it left behind: deposits,
withdrawals, bonuses, closed trades, financing, stop-outs, stop losses, take
profits and adjustments. The History tab in the client terminal is this.

**The balance column is the point of it.** A trader asking "why is my balance
this" is asking to follow it down the page, so each line carries the balance
*after* that movement, taken from the engine as it applied the event — not
re-added in the browser, where it could drift from what the engine thinks.

**Folded from the journal like everything else**, into a bounded window: two
thousand movements across all accounts, which is a long statement and a small
amount of memory. The journal has the rest.

**Opening a position is not a movement.** Nothing has been realised, so
nothing is on the statement until the position closes — which is also why
financing appears while a position is still open but moves no balance: it has
accrued, and the trader is owed the figure before they close.

**A session only ever sees its own account's statement.** The account comes
from the session, never from the message.

## Market data

### Gateway: four feeds, one price per instrument

The gateway takes prices from four providers and gives the engine exactly one
two-sided quote per instrument:

| Provider | Key | Streams | Used for |
|---|---|---|---|
| Binance | none | `@bookTicker` — best bid and ask | Crypto, primary |
| Kraken | none | v2 `ticker` — best bid and ask | Crypto, backup |
| Twelve Data | `TWELVEDATA_API_KEY` | `/v1/quotes/price` — price, and bid/ask on some plans | FX and metals, primary |
| Finnhub | `FINNHUB_API_KEY` | trades — last price only | FX (OANDA) backup; BTC and ETH third |

Keys go in `gateway/.env` (copy `gateway/.env.example`), which git ignores. They
are put into the WebSocket URL the gateway opens and nowhere else: never sent
to a browser, never logged — a failed connection logs the *kind* of error,
because a handshake error can echo the URL. A provider with no key does not
start, and says so once at startup.

**Each instrument lists its sources in priority order** in
`gateway/src/instruments.rs`. The aggregator (`feeds/aggregator.rs`) uses the
highest-priority source with a quote younger than the instrument's
`max_quote_age` — 5 s for crypto, 15 s for FX and metals.

- **Fail over at once, fail back slowly.** When the active source goes stale,
  the next fresh one takes over on its next tick. A returning higher-priority
  source must stay continuously fresh for 10 s before it takes the price back,
  so a feed dropping out every few seconds does not drag prices back and forth.
- **Last-price feeds get a configured spread.** Finnhub, and Twelve Data on
  plans without bid/ask, send one price. The instrument's `half_spread` is put
  either side of it. That makes them fine backups and poor primaries for
  anything traded on the spread, which is why they are ordered as they are.
- **What cannot be a market is refused, not repaired.** A crossed quote, a
  non-positive price, or a spread wider than the price is dropped and counted.
- **Every switch is journaled** as `PriceSourceChanged`, once per switch rather
  than per tick, so any fill can be traced to the feed that priced it.
- **No fill on a stale price.** A market order on an instrument whose last
  quote is older than its `max_quote_age` is rejected `marketClosed`. FX feeds go
  quiet on Friday night; filling Monday's first order at Friday's close would
  give someone the weekend gap for free.
- **A silent socket is reconnected.** A connection that delivers nothing for
  90 s is presumed half-open and replaced, with exponential backoff and jitter.

Prices from every provider are read as raw JSON tokens (`serde_json`'s
`RawValue`) and parsed straight into fixed-point, whether the provider sent
`"1.15387"` or `1.15387`. Exponent notation is refused rather than guessed at.

**Plan limits.** Twelve Data's free plan caps symbols per connection:
`TWELVEDATA_MAX_SYMBOLS` (default 8) subscribes tradeable instruments first and
logs the ones left out. Finnhub's is `FINNHUB_MAX_SYMBOLS` (default 50).

**Instruments quoted in another currency** are tradeable — see *Currency
conversion* below. USDJPY, USDCAD and USDCHF settle in yen, Canadian dollars and
francs, and each converts through the pair the trader is already trading.

The market watch tags each row with its current source (BN, KR, TD, FH) and the
status bar names it in full.

### Bars

The exchanges have real history for the crypto they list — hours of it, from
before this gateway ever ran — so those symbols still chart from Binance and
Kraken. For **FX, metals and the cross-quoted pairs there is nobody else to
ask**, so the gateway draws them itself.

**One-minute bars, folded upward.** Every tick updates the current minute;
longer intervals are folded from the minutes on request, because five
one-minute bars *are* a five-minute bar and storing both would be two numbers
that can disagree. Buckets align to the epoch, so a four-hour bar starts at
00:00 and 04:00 UTC like everyone else's — and like the same chart after a
reload.

**Every tick, not every journaled tick.** Quotes are journaled on a
quarter-second throttle; bars are not, because a bar's high and low are the
extremes the market actually printed and a throttle would quietly shave them
off.

**Rebuilt by replay.** Bars come back after a restart by re-reading the
journal's own quotes, stamped with the times they were journaled at — so a
restart draws the same chart rather than one stamped with the moment of the
replay. The history therefore goes back exactly as far as the journal, and a
fresh install has none: the terminal says it is filling in rather than drawing
a flat line.

**Mids, capped, and honest about intervals.** A chart is about where the market
was; which side a position closes on is the blotter's question. Twenty-five
hours of minutes are kept per instrument — about a megabyte for the whole
catalogue — and an interval the gateway does not serve is refused rather than
approximated, because a chart labelled 7m that is really 5m is worse than no
chart.

### On a phone

The desktop terminal is four panes at once, which is right at 1440px and
unusable at 390. Under 760px the same information stacks and scrolls: the
market watch first — prices are what a phone is opened for — then the chart,
then what the trader holds, with the deal ticket as a sheet across the bottom
where a thumb is. Rows grow to 34px, the toolbars scroll sideways rather than
wrapping the chart off the screen, and the status bar wraps instead of
overflowing.

**Nothing that carries money is hidden.** The Navigator goes, because it is
navigation the tabs already provide; every figure, the blotter and the status
bar stay.

### Terminal without the gateway

When the gateway is down, the terminal falls back to **free public exchange
feeds** directly — no API key, no account, and no trading:

| Provider | Quotes | Bars |
|---|---|---|
| Binance | `@bookTicker` over the combined WebSocket stream | `GET /api/v3/klines` |
| Kraken | v2 `ticker` channel | `GET /0/public/OHLC` |

`resilientFeed()` tries Binance first and falls back to Kraken when it cannot
connect — Binance is geo-restricted in the US and intermittently elsewhere, and
a trader there should still see prices without touching a setting. Once a
provider is live it stays chosen, including across its own reconnects: a
provider that reconnects has not failed, and flapping between exchanges
mid-session would change the prices under a trader's cursor.

**Prices never pass through a float.** Both exchanges publish decimal strings
(`"78389.03000000"`), which parse straight into fixed-point. The one exception
is Kraken's v2 WebSocket, which sends JSON numbers — `src/feed/kraken.ts`
documents why that is still exact at crypto magnitudes and why balances never
travel that path.

**Updates are conflated to 10 Hz.** BTCUSDT alone pushes several hundred book
tickers a second. Messages land in a mutable buffer and the component repaints
on a fixed cadence, keeping only the newest quote per instrument. Setting React
state per message would re-render the terminal hundreds of times a second and
show the trader nothing extra.

**Chart history is still exchange-only.** Bars come from Binance and Kraken, so
crypto has a chart and FX and metals show live prices with no bars until the
gateway serves history. `EURUSDT` stays listed as a crypto *proxy*, distinct
from the real EURUSD the gateway prices through Twelve Data.

**Data licensing.** Twelve Data and Finnhub plans each have their own terms for
displaying data to end users. Check that the plan in use permits showing its
prices to your clients before going live — a personal or free plan usually
does not.

## Build

### Engine

Targets the **GNU** toolchain on Windows, because MSVC build tools are not
installed on the current development machine. Either target works; pick one per
machine and stay on it.

```sh
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu

cd engine
cargo test --workspace
cargo clippy --workspace --all-targets
```

### Gateway

The gateway needs MinGW's `dlltool` on `PATH`, because the GNU toolchain uses it
to build the crates that link Windows import libraries. It is installed at:

```
%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin
```

```powershell
$mingw = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$env:PATH = "$env:USERPROFILE\.cargo\bin;$mingw;$env:PATH"

cd gateway
copy .env.example .env    # then put your Twelve Data and Finnhub keys in .env
cargo run                 # ws://127.0.0.1:8081, journal in ./journal
cargo run -- D:\data      # or a directory of your choosing

$env:FERARO_SEGMENT_BYTES = 4096   # roll often, to watch it happen
$env:FERARO_SYNC_MS = 200          # longest a quote may stay unsynced
$env:FERARO_ROLLOVER_HOUR = 22     # UTC hour of the daily swap charge
$env:FERARO_USERS = "D:\data\users.json"   # default: <journal>/users.json
$env:FERARO_SNAPSHOT_EVERY = 10000 # events between snapshots; 0 turns them off
```

**First run prints two passwords** — one `admin`, one `demo` — and never shows
them again. They are not stored in plain form anywhere, so a lost one is reset
rather than recovered. Sign in with them at the terminal.

Delete the journal directory to start the accounts over; the user store goes
with it, and the next run seeds fresh logins. Keep it to keep your positions.

### Web

```sh
cd web
npm install
npm run dev      # http://localhost:5173
npm test         # fixed-point parity with the engine
npm run build    # type-check, then bundle
```

The terminal opens on a sign-in screen and shows nothing else until the gateway
accepts a login — because the gateway answers nothing until it does. Signed in,
it trades against the real engine. With the gateway down, the screen says so
and keeps trying to reconnect; it does not invent an account.

If `cargo test` fails to link with `link: missing operand`, a Unix `link.exe`
from Git Bash is shadowing the real linker. Build from PowerShell, or put the
toolchain's `bin` ahead of `/usr/bin` on `PATH`.

## Running it on a server

```sh
cp gateway/.env.example gateway/.env        # feed keys; optional
FERARO_DOMAIN=demo.example.com docker compose up -d
docker compose logs gateway                 # the two seeded passwords, once
```

Three containers. **Caddy** terminates TLS and is the only thing with a port
on the internet; it fetches and renews a Let's Encrypt certificate on its own,
which matters because the platform's worst gap was a password crossing a
network in clear and the fix must not be something somebody remembers to redo
in ninety days. **The gateway** publishes no ports at all — it is reachable
from Caddy and nowhere else, because it speaks plain `ws://`. **The terminals**
are static files Caddy serves.

The browser finds the socket from the page's own origin — `wss://host/ws` —
so one build works on a laptop and on a server, and an `https://` page can
never open a `ws://` socket because a constant was left behind.

### Knowing it is alive

`/health` answers `200` while prices are arriving and `503` once they have
stopped, because "the process is running" is not the question a supervisor is
asking: a gateway whose feeds stalled is up and useless. It reports uptime,
how many prices have been seen and how long since the last one. Docker's
health check calls `gateway --healthcheck`, which lives in the binary because
the runtime image has no `curl` and should not grow one.

It is bound to loopback and the proxy serves it only to private addresses.

### Backing up

**The journal is the database.** Every balance, position and decision is a fold
over it, so copying that one directory copies the business — and losing it
loses who owns what money.

```sh
ops/backup.sh                                   # hourly, from cron
ops/restore.sh /var/backups/feraro/journal-....tar.gz
```

The journal is append-only and fsynced on a timer, so a copy taken while the
gateway runs is a copy of a *prefix* of the log — which replays. A torn tail
is indistinguishable from a crash, and the engine already refuses a corrupt
segment rather than serving a wrong balance from it, so the backup needs no
downtime.

`restore.sh` refuses to overwrite a journal that has anything in it, and
proves the backup by replaying it: a gateway that starts and reports healthy
has verified the archive, which is the only verification worth having.

Two things the scripts cannot do for you. **Copy the tarballs off the
machine** — a backup on the same disk as the thing it backs up is not a
backup. And **restore one on purpose, before you need to**; the first time you
find out whether it works must not be the morning the disk died.

### What is deliberately not in an image

`.env`, `users.json`, `payments.json`, the journal and the snapshots. A key
copied into a layer is a key in the image whatever the Dockerfile does
afterwards, and a journal baked into one means a redeploy quietly reverts
every balance to whenever that image was built.

## The invariants

These are asserted by the test suite, not merely documented. Each has an ADR in
`docs/adr/` explaining what goes wrong without it.

1. **No floating point touches money.** Every price, quantity and balance is a
   scaled integer, and every lossy operation rounds explicitly, half away from
   zero. Truncation toward zero biases every rounded P&L toward the house.
2. **Events are the only truth.** Engine state is a fold over the event stream.
   State not reachable by replaying from genesis does not exist.
3. **The journal is the database.** The trade path writes here and nowhere else.
   PostgreSQL is fed asynchronously and is never read while matching.
4. **Every currency nets to zero.** A transaction that does not balance *per
   currency* is refused before it touches a balance — and the books are folded
   from the journal beside the portfolio, so client cash equals the account's
   balance after every event, not merely in the ledger's own tests.
5. **One writer, no locks.** The book is owned by a single thread applying
   orders in a serialised loop. This makes the two-orders-one-balance race
   unreachable rather than unlikely.
6. **No order reaches the book the account cannot afford.** `check_order` runs
   before matching, on the same thread, against state that thread owns.
7. **Margin required is a difference, not a total.** An order needs
   `margin(after) − margin(before)`, floored at zero, so a *closing* order needs
   nothing. An account past its stop-out level must always be able to reduce
   risk; a naive notional/leverage check refuses exactly that and traps the
   trader in the position.
8. **Every figure on an account is in the account's currency.** Amounts arising
   in an instrument's quote currency are converted on the way in, at a rate that
   is journaled with the decision that used it. An instrument that cannot be
   converted cannot be traded.
9. **No account is acted on without a session entitled to it.** The account a
   command carries comes from the signed-in session, never from the message;
   the engine independently checks that a named ticket is on that account; and
   nothing — not even a price — is broadcast to a connection that has not
   signed in.

## What is deliberately not here yet

Named so that nobody mistakes an absence for an oversight:

- **Self-match prevention** — P5. It cannot trigger on a pure B-book, where the
  house is always the counterparty.
- **Multi-currency accounts and cross-currency routes.** Every account settles in
  US dollars, and each quote currency reaches it through exactly one pair. An
  account in euros, or an instrument quoted in a currency with no direct dollar
  pair, needs two rates multiplied and a settlement currency per account.
- **A conversion spread or markup.** Clients convert at the mid, which is better
  than most brokers give them. Charging for it means a visible fee in the ledger.
- **Day and fill-or-kill on resting orders.** Everything rests good-till-
  cancelled; nothing expires at the end of the session. A Day order needs an
  expiry sweep at the trading-day boundary, which the rollover already walks.
- **Trailing stops.** A stop that follows the market needs a rule for *when* it
  moves — every tick, every so many points — and that rule is journaled state
  like any other.
- **Journal compaction.** Snapshots bound the startup *time*; nothing yet
  deletes or archives the segments a snapshot has made redundant, so the disk
  still grows with history. Compaction has to decide what a regulator may ask
  for and when, which is a retention policy before it is code.
- **Transport security.** Sign-in is real, but the socket is plain `ws://` on
  localhost. A password must not cross a network without TLS, so a deployment
  needs `wss://` and a certificate before this is reachable from anywhere else.
- **A sign-in audit trail.** *Actions* are attributed — who deposited, who
  placed an order on whose account. Sign-ins themselves are not: there is no
  record of who connected, from where, or when they failed to.
- **A payment provider.** Money is paid to a wallet or a bank the desk
  publishes, and credited by hand once somebody has seen it arrive. Nothing
  reconciles automatically, and nothing pays out: a withdrawal is an
  administrator moving the money themselves and recording it.
- **A licence, and identity documents.** Taking client money needs
  authorisation, and this collects no document to justify a verification.
  Both are named here rather than implied by silence, and both are why the
  honest description of this today is a demo platform.
- **Dealer terminal.** Deliberately not built: desk work lives in the manager
  terminal, which `manager` and `admin` share.
- **Per-manager permissions.** What a manager may do is one fixed split, the
  same for all of them, rather than a grant an administrator tunes per person.
  A matrix is the natural next step; a fixed, stated line is the honest first
  one, because a permission nobody can enumerate is a permission nobody
  audits.
- **Commission.** The reference blotter has a commission column; this one
  does not, because no commission is charged. A column of zeros implies a
  feature, so there is no column.
- **Commission and introducing-broker books.** A manager has a book of clients
  but earns nothing from it: there is no commission account, no split, and no
  statement of what a manager is owed. That is ledger work, and it needs the
  commercial terms before the code.
- **Identity documents.** A client can be marked verified; nothing collects or
  checks a document to justify it. See *Identity checks*.
- **A resizable client terminal.** The panes that move and close are the
  manager's. The client terminal still has its fixed layout; the same
  `Splitter` and layout store would do it.
- **The blotter's quieter tabs.** Journal, Mailbox, News and Alerts are framed
  and empty. Each needs a source behind it — a message store, a news feed —
  rather than markup, so they are left visibly unfinished instead of filled
  with something invented.
- **Exports.** The statement and the books read on screen and nowhere else. A
  CSV is easy; a statement a client can hand to an accountant is a format
  decision, and nobody has made it.
