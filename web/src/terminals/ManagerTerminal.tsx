/**
 * The manager's terminal: the desk's view of the platform.
 *
 * # The layout, and why it is this one
 *
 * One screen, four regions, the arrangement every dealing desk already knows:
 *
 * ```
 *  ┌──────────┬────────────────────────┬──────────────┐
 *  │ clients  │        chart           │ market watch │
 *  │ Live     │   with the deal ticket │              │
 *  │ Demo     ├────────────────────────┴──────────────┤
 *  │          │  balance · equity · margin · profit   │
 *  ├──────────┴───────────────────────────────────────┤
 *  │ toolbox: accounts · positions · people · books   │
 *  └──────────────────────────────────────────────────┘
 * ```
 *
 * Clients on the left because that is what a manager navigates by; the chart
 * in the middle because it is the thing you look *at*; prices on the right
 * where the client terminal has them, so muscle memory carries over; the
 * account's figures on a strip under both, because they qualify everything
 * above them; and the toolbox along the bottom because its tables are wide
 * and shallow.
 *
 * # One screen, two roles
 *
 * A manager and an administrator get the *same* screen. The difference is
 * the book behind it: an administrator sees every account, a manager sees
 * the clients assigned to them, and the gateway decides which — this file
 * only hides what it knows would be refused, so a manager is not offered a
 * button that would fail.
 *
 * What a manager may not do is move money in or out, make an account live,
 * decide identity, create a login, or read the platform's books and audit
 * trail. Those make a book rather than work one. Hiding them here is a
 * courtesy; `desk_refusal` in the gateway is the rule.
 *
 * # Everything on one screen
 *
 * There is no second terminal to open. The desk deals from the ticket over
 * the chart, on the account the tree has picked out, and manages what it
 * opened from the Positions tab below — closing, and setting a level, in the
 * place the position is already listed. A button that takes an operator to
 * another screen to do half the job is a button that loses them the price.
 *
 * # Picking a client selects their account
 *
 * Clicking an account in the tree switches the session to it, so the toolbox
 * shows *their* positions, *their* statement and *their* figures. This is the
 * same `selectAccount` a client's own account picker uses, and the same
 * authorisation decides it: an administrator may act on any account, and every
 * action they take is journaled with their name on it.
 *
 * # Nothing here is computed
 *
 * Every figure comes from the engine, which is the only thing entitled to say
 * what an equity or a margin level is. The browser formats, and nothing else.
 */

import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { Ask } from '../components/Ask'
import type { AskField } from '../components/Ask'
import { ChartPane, TF_LABEL } from '../components/ChartPane'
import { ContextMenu } from '../components/ContextMenu'
import type { MenuItem, MenuLine, MenuSubject } from '../components/ContextMenu'
import type { Rail } from '../feed/gateway'
import { DealTicket } from '../components/DealTicket'
import type { Resting } from '../components/DealTicket'
import { LevelCell } from '../components/LevelCell'
import { MarketWatch } from '../components/MarketWatch'
import { PricingSheet } from '../components/PricingSheet'
import { SecondFactor } from '../components/SecondFactor'
import { RequestQueue } from '../components/RequestQueue'
import { Sheet } from '../components/Sheet'
import { useChimes } from '../feed/useChimes'
import { isMuted, play, setMuted, wake } from '../lib/chime'
import { Splitter } from '../components/Splitter'
import { DataTable, Pill } from '../components/primitives'
import type { Column } from '../components/primitives'
import type {
  AdminAccount,
  GatewayPending,
  AdminActionKind,
  AmendReason,
  AuditLine,
  GatewayPosition,
  Kyc,
  MoneyKind,
} from '../feed/gateway'
import type { FeedQuote, Interval } from '../feed/types'
import type { GatewayView } from '../feed/useGateway'
import type { Instrument } from '../lib/domain'
import { DEFAULT_INSTRUMENT, INSTRUMENTS } from '../lib/instruments'
import type { Layout, PanesOpen } from '../lib/layout'
import { clampPane, DEFAULT_LAYOUT, readLayout, toggled, writeLayout } from '../lib/layout'
import type { Money, Price, Qty } from '../lib/money'
import {
  price as parsePrice,
  QTY_ONE,
  showMarginLevel,
  showMoney,
  showPrice,
  showQty,
  showQtyAsLots,
  showSigned,
  qty,
} from '../lib/money'
import './ManagerTerminal.css'

/**
 * Lots as a quantity, and back.
 *
 * The blotter's Volume column is lots — size divided by the contract — so
 * the figure somebody reads off a row is the figure they should be able to
 * type back. The wire deals in quantities, so the conversion happens here
 * rather than being left to whoever is looking at the screen.
 */
const asQty = (lots: Qty, contract: Qty): Qty =>
  ((lots * contract) / QTY_ONE) as Qty

/** The ellipsis a menu label ends with when it opens a dialog. */
const DOTS = '\u2026'

/** How often the book refreshes itself while the screen is open. */
const REFRESH_MS = 4_000

type Tab = 'Trade' | 'History' | 'Net Trade' | 'Requests' | 'Audit'

/**
 * Why money moved, per kind of movement.
 *
 * Chosen from a list rather than typed, because the journal's wire format
 * is fixed-width: there is no variable-length string in it, so a free-text
 * description would be collected and then thrown away. A known set also
 * makes the audit trail countable, which prose never is.
 */
const REASONS: Record<string, readonly { value: string; label: string }[]> = {
  deposit: [
    { value: 'Deposit', label: 'Deposit' },
    { value: 'Bank transfer', label: 'Bank transfer' },
    { value: 'Crypto', label: 'Crypto' },
    { value: 'UPI', label: 'UPI' },
    { value: 'Correction', label: 'Correction' },
  ],
  withdraw: [
    { value: 'Withdrawal', label: 'Withdrawal' },
    { value: 'Bank transfer', label: 'Bank transfer' },
    { value: 'Crypto', label: 'Crypto' },
    { value: 'Correction', label: 'Correction' },
  ],
  credit: [
    { value: 'Bonus', label: 'Bonus' },
    { value: 'Promotion', label: 'Promotion' },
    { value: 'Goodwill', label: 'Goodwill' },
    { value: 'Removed', label: 'Removed' },
  ],
}

/** What each payment rail is called on screen. */
const RAIL_WORD: Record<string, string> = {
  crypto: 'Crypto',
  upi: 'UPI',
  bank: 'Bank',
}

/** The folder for accounts whose client no manager holds. */
const UNASSIGNED = '\u0000unassigned'

/** The panes, and what the View menu calls them. */
const PANES: readonly (readonly [keyof PanesOpen, string])[] = [
  ['navigator', 'Navigator'],
  ['chart', 'Chart'],
  ['watch', 'Market watch'],
  ['toolbox', 'Toolbox'],
]

/** The toolbox tabs, by role. The books and the audit trail are the
 *  platform's: a manager reading either would be reading other managers'
 *  clients. */
const TABS: Readonly<Record<'admin' | 'manager', readonly Tab[]>> = {
  admin: ['Trade', 'History', 'Net Trade', 'Requests', 'Audit'],
  manager: ['Trade', 'History', 'Net Trade', 'Requests'],
}

export interface ManagerTerminalProps {
  readonly gw: GatewayView
}

export function ManagerTerminal({ gw }: ManagerTerminalProps) {
  const [tab, setTab] = useState<Tab>('Trade')
  /**
   * The open charts, and which one is in front.
   *
   * Each tab keeps its own timeframe, because a tab *is* a chart: flipping
   * to the hourly on gold and back to find bitcoin also on the hourly is
   * the sort of small lie that costs a trade.
   */
  const [charts, setCharts] = useState<readonly { id: number; interval: Interval }[]>([
    { id: DEFAULT_INSTRUMENT.id, interval: '5m' },
  ])
  const [symbol, setSymbol] = useState(DEFAULT_INSTRUMENT.id)
  const [find, setFind] = useState('')
  const [watchTab, setWatchTab] = useState<'Market Watch' | 'Ticks'>('Market Watch')
  /** Which panes are open and how wide, remembered across reloads. */
  const [layout, setLayout] = useState<Layout>(readLayout)
  useEffect(() => writeLayout(layout), [layout])

  const show = (pane: keyof PanesOpen) =>
    setLayout((l) => ({ ...l, open: toggled(l.open, pane) }))
  const resize = (which: 'nav' | 'watch' | 'toolbox', delta: number) =>
    setLayout((l) => ({ ...l, [which]: clampPane(which, l[which] + delta) }))

  /** The open right-click menu, if there is one. */
  const [menu, setMenu] = useState<{
    x: number
    y: number
    subject: MenuSubject
    items: readonly MenuItem[]
  } | null>(null)

  /** The open dialog, if there is one. */
  const [ask, setAsk] = useState<{
    title: string
    /** Who or what it acts on, under the title. */
    subject?: string
    /** One glyph, in the disc beside the title. */
    icon?: string
    note?: string
    fields: readonly AskField[]
    submitLabel: string
    tone?: 'danger'
    run(values: Record<string, string>): void
  } | null>(null)

  const [viewOpen, setViewOpen] = useState(false)
  const [quiet, setQuiet] = useState(isMuted)

  // Fills, stop-outs, margin calls and protections.
  useChimes(gw.events, gw.positions)

  /**
   * Clients arriving and leaving.
   *
   * There is no push for this — a sign-in is journaled and shown in the
   * activity feed, which the desk polls. So the feed is polled on the
   * ordinary refresh and sounded when its newest entry changes, which is
   * as close to live as the record gets without a new wire message.
   */
  const heardActivity = useRef<number | null>(null)
  useEffect(() => {
    const newest = gw.activity[0]
    if (!newest) return
    const was = heardActivity.current
    heardActivity.current = newest.seq
    // The first poll after opening the screen is history, not news.
    if (was === null || newest.seq <= was) return
    if (newest.what === 'signedIn' || newest.what === 'resumed') play('signIn')
    if (newest.what === 'signedOut') play('signOut')
    if (newest.what === 'refused') play('refused')
  }, [gw.activity])

  /** A read-only panel, when one has been opened from a menu. */
  /**
   * Which population the navigator is showing.
   *
   * Live first, because that is the one where a mistake costs money and
   * so the one somebody should have to switch *away* from rather than
   * towards.
   */
  const [side, setSide] = useState<'live' | 'demo'>('live')
  const [sheet, setSheet] = useState<
    'books' | 'payments' | 'pricing' | 'second' | null
  >(null)

  /**
   * Tickets ticked in the blotter.
   *
   * Held by number rather than by row, so a tick survives the four-second
   * refresh — a selection that clears itself while you are reaching for the
   * button is worse than no selection at all. Tickets that have since
   * closed simply stop matching.
   */
  const [ticked, setTicked] = useState<ReadonlySet<number>>(new Set())
  const tick = (ticket: number) =>
    setTicked((prev) => {
      const next = new Set(prev)
      if (next.has(ticket)) next.delete(ticket)
      else next.add(ticket)
      return next
    })

  /**
   * True for an administrator.
   *
   * Used only to decide what to *offer*. Every one of these actions is
   * checked again by the gateway, which does not trust a screen.
   */
  const isAdmin = gw.session?.role === 'admin'

  /** The account the session is acting on — the one the tree has picked out. */
  const selected = gw.session?.account ?? 0

  const instruments: readonly Instrument[] = gw.instruments.length
    ? gw.instruments
    : INSTRUMENTS
  const instrument = instruments.find((i) => i.id === symbol) ?? DEFAULT_INSTRUMENT
  const quote = gw.quotes.get(instrument.id)

  /**
   * The net of what is open, one line per symbol.
   *
   * A blotter lists tickets; this answers the question a desk actually asks
   * of a client's book — *which way are they, and by how much*. Long and
   * short on the same symbol are netted whether or not the account is in
   * hedging mode, because the exposure is the same either way; hedging
   * changes what margin it costs, not which way the money moves.
   */
  const netTrade = useMemo(() => {
    const by = new Map<number, { long: Qty; short: Qty; profit: Money; swap: Money }>()
    for (const p of gw.positions) {
      const at = by.get(p.instrumentId) ?? {
        long: 0n as Qty,
        short: 0n as Qty,
        profit: 0n as Money,
        swap: 0n as Money,
      }
      by.set(p.instrumentId, {
        long: (p.side === 'buy' ? at.long + p.qty : at.long) as Qty,
        short: (p.side === 'sell' ? at.short + p.qty : at.short) as Qty,
        profit: (at.profit + p.profit) as Money,
        swap: (at.swap + p.swap) as Money,
      })
    }
    return [...by.entries()]
      .map(([instrumentId, held]) => ({
        instrumentId,
        ...held,
        net: (held.long - held.short) as Qty,
      }))
      .sort((x, y) => symbolOf(instruments, x.instrumentId).localeCompare(symbolOf(instruments, y.instrumentId)))
  }, [gw.positions, instruments])


  useEffect(() => {
    gw.adminOverview()
    const timer = window.setInterval(() => {
      gw.adminOverview()
      if (tab === 'Audit') gw.adminAudit()
      if (tab === 'History') gw.requestStatement()
      if (tab === 'Requests') gw.adminDepositRequests()
      // Not for a tab — for the sound. The desk wants to hear a client
      // arrive whichever screen it is looking at.
      if (!quiet) gw.adminActivity()
    }, REFRESH_MS)
    return () => window.clearInterval(timer)
  }, [gw, tab, quiet])

  // A role that no longer has this tab — a demotion while signed in — is put
  // back on one it does have rather than left on an empty pane.
  useEffect(() => {
    if (!TABS[isAdmin ? 'admin' : 'manager'].includes(tab)) setTab('Trade')
  }, [isAdmin, tab])

  // The tab's own data, fetched when it is opened rather than polled always.
  useEffect(() => {
    if (tab === 'Audit') gw.adminAudit()
    if (tab === 'History') gw.requestStatement()
    if (tab === 'Requests') gw.adminDepositRequests()
  }, [tab, gw, selected])

  const account = useMemo(
    () => gw.book.find((a) => a.id === selected) ?? null,
    [gw.book, selected],
  )

  // A tick was about this account's position, so it does not follow you to
  // the next account.
  useEffect(() => setTicked(new Set()), [selected])

  /**
   * A login created as a manager, waiting for the book to tell us its id.
   *
   * Creating returns a message, not a record, so the promotion happens on
   * the next overview — which also means a failed creation simply never
   * promotes anybody.
   */
  const [promote, setPromote] = useState<string | null>(null)
  useEffect(() => {
    if (promote === null) return
    const made = gw.users.find((u) => u.login.toLowerCase() === promote)
    if (!made) return
    setPromote(null)
    if (made.role === 'client') gw.adminSetRole(made.id, 'manager')
  }, [promote, gw])

  /** The managers a client can be handed to. */
  const managers = useMemo(() => gw.users.filter((u) => u.role === 'manager'), [gw.users])


  /**
   * What the selected account's open positions are worth right now.
   *
   * Price and financing kept apart as well as together, because they are
   * different money: one moves with the market, the other was charged.
   */
  const openPrice = useMemo(
    () => gw.positions.reduce((acc, p) => acc + p.profit, 0n) as Money,
    [gw.positions],
  )
  const openSwap = useMemo(
    () => gw.positions.reduce((acc, p) => acc + p.swap, 0n) as Money,
    [gw.positions],
  )
  const openProfit = (openPrice + openSwap) as Money

  /**
   * The navigator's list: live or demo, narrowed by what was typed.
   *
   * Flat rather than nested, because a desk looks an account up by its number
   * or its client's address far more often than it browses a client's
   * holdings — and the two tabs already answer the question nesting would.
   */
  /** One account, with the facts the tree draws beside it. */
  const decorate = (a: AdminAccount) => {
    const owner = gw.users.find((u) => u.id === a.owner)
    return {
      account: a,
      kyc: owner?.kyc ?? ('none' as Kyc),
      manager: owner?.managerLogin ?? null,
    }
  }

  const matches = useMemo(() => {
    const needle = find.trim().toLowerCase()
    if (needle === '') return gw.book
    return gw.book.filter(
      (a) =>
        String(a.id).includes(needle) ||
        (a.ownerLogin ?? '').toLowerCase().includes(needle),
    )
  }, [gw.book, find])

  /**
   * The navigator's tree: accounts under the manager who holds them.
   *
   * The folder *is* the manager, which is the arrangement the desk actually
   * has. Accounts nobody has been given sit under **Unassigned**, named
   * rather than hidden: an account with no manager is a fact somebody should
   * see, not an absence.
   *
   * Live and demo are separated inside each folder because they are
   * different kinds of thing — one holds somebody's money.
   */
  const tree = useMemo(() => {
    const byManager = new Map<string, AdminAccount[]>()
    for (const a of matches.filter((a) => (side === 'live' ? a.live : !a.live))) {
      const owner = gw.users.find((u) => u.id === a.owner)
      const key = owner?.managerLogin ?? (owner?.role === 'manager' ? owner.login : null)
      byManager.set(key ?? UNASSIGNED, [...(byManager.get(key ?? UNASSIGNED) ?? []), a])
    }
    return [...byManager.entries()]
      .map(([manager, held]) => ({
        manager,
        live: held.filter((a) => a.live).sort((x, y) => x.id - y.id).map(decorate),
        demo: held.filter((a) => !a.live).sort((x, y) => x.id - y.id).map(decorate),
      }))
      // A manager whose whole book is on the other side is not shown at
      // all: a folder with nothing in it is a folder somebody opens.
      .filter((f) => f.live.length + f.demo.length > 0)
      // Unassigned last: it is the exception, and exceptions read better at
      // the bottom of a list than at the top of one.
      .sort((x, y) =>
        x.manager === UNASSIGNED
          ? 1
          : y.manager === UNASSIGNED
            ? -1
            : x.manager.localeCompare(y.manager),
      )
  }, [matches, gw.users, side])

  const favourites = useMemo(
    () =>
      layout.favourites
        .map((id) => matches.find((a) => a.id === id))
        .filter((a): a is AdminAccount => a !== undefined)
        .map(decorate),
    [layout.favourites, matches],
  )

  const isFavourite = (id: number) => layout.favourites.includes(id)
  const star = (id: number) =>
    setLayout((l) => ({
      ...l,
      favourites: l.favourites.includes(id)
        ? l.favourites.filter((f) => f !== id)
        : [...l.favourites, id],
    }))

  /** Which branches are shut. Everything is open until it is closed. */
  const [shut, setShut] = useState<ReadonlySet<string>>(new Set())
  const fold = (key: string) =>
    setShut((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })

  /**
   * An order on the account the tree has picked out.
   *
   * The ticket has already worked out whether this is a market order or a
   * resting one — that is what its arrows do — so this only has to confirm
   * it and send it. Confirmed by name and number, because the operator is
   * dealing on somebody else's money and a misread row is how that goes
   * wrong.
   */
  const deal = (order: Resting, volume: Qty) => {
    if (!account) return
    const who = account.ownerLogin ?? `account ${account.id}`
    const lots = showQtyAsLots(volume, instrument.contractSize, 3)
    const way = order.side === 'buy' ? 'Buy' : 'Sell'
    const what =
      order.kind === 'market'
        ? `${way} ${lots} ${instrument.symbol} at the market`
        : `Rest a ${way.toLowerCase()} ${order.kind} for ${lots} ${instrument.symbol} at ${showPrice(order.price, instrument.displayDigits)}`
    if (!window.confirm(`${what} on account ${account.id} — ${who}?`)) return

    gw.placeOrder({
      instrumentId: instrument.id,
      side: order.side,
      kind: order.kind,
      qty: volume,
      ...(order.kind === 'market' ? {} : { price: order.price }),
    })
  }

  /**
   * Where the ticket's two sides are currently pointed.
   *
   * Drawn on the chart so a price you are about to rest is seen against the
   * candles before it is sent, not after.
   */
  const [aiming, setAiming] = useState<readonly Resting[]>([])

  /** What a tab has in it, shown beside its name. */
  const tabCount = (t: Tab): string | null => {
    switch (t) {
      case 'Trade':
        return `${ticked.size}/${gw.positions.length}`
      case 'History':
        return String(gw.statement.length)
      case 'Net Trade':
        return String(netTrade.length)
      case 'Requests':
        return String(gw.deposits.filter((d) => d.stage === 'pending').length)
      default:
        return null
    }
  }

  /** The ticked positions that are still open. */
  const chosen = useMemo(
    () => gw.positions.filter((p) => ticked.has(p.ticket)),
    [gw.positions, ticked],
  )

  const closeChosen = () => {
    if (
      !window.confirm(
        `Close ${chosen.length} position${chosen.length === 1 ? '' : 's'} on account ${selected}?`,
      )
    )
      return
    for (const p of chosen) gw.closePosition(p.ticket)
    setTicked(new Set())
  }

  /**
   * Opens the opposite side beside each ticked position.
   *
   * Only on an account that hedges: on a netting account the opposite order
   * closes the position instead of standing beside it, which is the
   * opposite of what the word means. The button says so rather than doing
   * the wrong thing quietly.
   */
  const hedgeChosen = () => {
    if (!account?.hedging) return
    if (
      !window.confirm(
        `Open the opposite side of ${chosen.length} position${chosen.length === 1 ? '' : 's'} on account ${selected}? Both sides stay open.`,
      )
    )
      return
    for (const p of chosen) {
      gw.placeOrder({
        instrumentId: p.instrumentId,
        side: p.side === 'buy' ? 'sell' : 'buy',
        kind: 'market',
        qty: p.qty,
      })
    }
    setTicked(new Set())
  }

  const cancelResting = () => {
    if (!window.confirm(`Cancel ${gw.pending.length} resting order(s) on account ${selected}?`))
      return
    for (const o of gw.pending) gw.cancelOrder(o.ticket)
  }

  /**
   * What a right-click offers on an account.
   *
   * Grouped the way a desk thinks about an account rather than the way the
   * protocol is shaped: money, dealing, the person, then the switches, then
   * the things you cannot take back. Everything here has this one account
   * as its subject, which is why no line has to name it — the header did.
   *
   * A manager is offered less, matching what the gateway would allow. That
   * is a courtesy; `desk_refusal` is the rule.
   */
  const accountMenu = (a: AdminAccount): readonly MenuItem[] => {
    const who = a.ownerLogin ?? `account ${a.id}`
    const owner = gw.users.find((u) => u.id === a.owner)
    const after = (ms = 150) => window.setTimeout(() => gw.adminOverview(), ms)

    const money = (
      kind: 'deposit' | 'withdraw' | 'credit',
      label: string,
      icon: string,
      note: string,
    ): MenuLine => ({
      label,
      icon,
      onPick: () =>
        setAsk({
          title: label,
          subject: `${a.id} \u2014 ${who}`,
          icon: kind === 'credit' ? '\u25c8' : kind === 'withdraw' ? '\u2191' : '\u2193',
          note,
          fields: [
            {
              key: 'amount',
              label: 'Amount',
              placeholder: '0.00',
              mode: 'decimal' as const,
              beside: true,
            },
            {
              key: 'why',
              label: 'Reason',
              beside: true,
              // Chosen, not typed. The journal's wire format is fixed-width
              // \u2014 there is no variable-length string in it \u2014 so free text
              // would be collected and then dropped, which is worse than
              // not asking. A known set also counts, which prose never does.
              choices: REASONS[kind] ?? [],
            },
          ],
          submitLabel: 'Apply',
          ...(kind === 'withdraw' ? { tone: 'danger' as const } : {}),
          run: ({ amount, why }) => {
            if (
              !window.confirm(
                `${label} ${(amount ?? '').trim()} ${kind === 'withdraw' ? 'from' : 'to'} account ${a.id} \u2014 ${who}?\n\nReason: ${why}`,
              )
            )
              return
            gw.adminAction(a.id, kind, (amount ?? '').trim())
            after()
          },
        }),
    })

    /**
     * A movement the desk starts, which still has to be approved.
     *
     * Unlike `money` above it does not apply anything: it raises a request
     * in the same queue a client's deposit lands in. That is deliberate.
     * Bonus and insurance are the two movements an operator is most likely
     * to get wrong, and a queue means a second pair of eyes and one place
     * where every movement on this account can be read in order.
     */
    const queued = (
      kind: MoneyKind,
      label: string,
      icon: string,
      note: string,
    ): MenuLine => ({
      label,
      icon,
      onPick: () =>
        setAsk({
          title: label,
          subject: `${a.id} \u2014 ${who}`,
          icon,
          note,
          fields: [
            {
              key: 'amount',
              label: 'Amount',
              placeholder: '0.00',
              mode: 'decimal' as const,
              beside: true,
            },
            {
              key: 'note',
              label: 'Note',
              beside: true,
              required: false,
              mode: 'text' as const,
            },
          ],
          submitLabel: 'Queue it',
          run: ({ amount, note: why }) => {
            gw.adminRaise(kind, a.id, (amount ?? '').trim(), (why ?? '').trim())
            // It is in the queue, not applied. Showing the queue next is
            // the honest thing: the operator sees that nothing moved yet.
            window.setTimeout(() => {
              gw.adminDepositRequests()
              gw.adminOverview()
            }, 200)
            setTab('Requests')
          },
        }),
    })

    const plain = (kind: AdminActionKind, label: string, icon?: string): MenuLine => ({
      label,
      ...(icon ? { icon } : {}),
      onPick: () => {
        if (!window.confirm(describeAction(kind, a.id, ''))) return
        gw.adminAction(a.id, kind, '')
        after()
      },
    })

    /** Every account this client holds — what "all accounts" means. */
    const theirs = gw.book.filter((x) => x.owner !== null && x.owner === a.owner)
    const sweep = (kind: AdminActionKind, what: string) => () => {
      const hit = theirs.filter((x) => (kind === 'suspend' ? x.tradeable : !x.tradeable))
      if (hit.length === 0) return
      if (!window.confirm(`${what} all ${hit.length} of ${who}'s accounts?`)) return
      for (const x of hit) gw.adminAction(x.id, kind, '')
      after(250)
    }

    const items: MenuItem[] = []

    // ── Money, dealing, and another account ──
    if (isAdmin) {
      items.push({
        label: 'Money',
        icon: '\u25c9',
        items: [
          money('deposit', 'Deposit', '\u2193', `Pays money into ${a.id} \u2014 ${who}.`),
          money('withdraw', 'Withdraw', '\u2191', `Takes money out of ${a.id} \u2014 ${who}. Bonus cannot be withdrawn.`),
          money('credit', 'Set bonus', '\u25c8', `Sets the non-withdrawable bonus on ${a.id} to this figure, replacing what is there.`),
          {
            label: 'Move to another account' + DOTS,
            icon: '\u21c4',
            // Only from the selected account: the gateway sends from
            // whichever account the session is on, so offering it from a
            // row that is not selected would send from the wrong one.
            disabled: a.id !== selected,
            hint: a.id === selected ? undefined : 'select it first',
            onPick: () =>
              setAsk({
                title: 'Move money',
                subject: `${a.id} \u2014 ${who}`,
                icon: '\u21c4',
                note: `Moves money out of ${a.id} and into another account. Both legs are one journal record, so there is no moment when the money is in neither. Demo and live do not mix, and the two accounts must hold the same currency.`,
                fields: [
                  {
                    key: 'to',
                    label: 'To account',
                    placeholder: '1002',
                    mode: 'numeric' as const,
                    beside: true,
                  },
                  {
                    key: 'amount',
                    label: 'Amount',
                    placeholder: '0.00',
                    mode: 'decimal' as const,
                    beside: true,
                  },
                ],
                submitLabel: 'Move it',
                run: ({ to, amount }) => {
                  const target = Number((to ?? '').trim())
                  if (!Number.isFinite(target) || target <= 0) return
                  const sending = (amount ?? '').trim()
                  if (
                    !window.confirm(
                      `Move ${sending} from account ${a.id} \u2014 ${who} \u2014 to account ${target}?`,
                    )
                  )
                    return
                  gw.transfer(target, sending)
                  after(250)
                },
              }),
          },
          '-',
          queued(
            'insuranceIn',
            'Set aside\u2026',
            '\u25a0',
            `Moves ${who}'s own money out of the market's reach. It stops supporting margin, so their free margin falls by it. Goes in the queue to be approved.`,
          ),
          queued(
            'insuranceOut',
            'Release\u2026',
            '\u25a1',
            `Puts money that was set aside back into ${a.id}'s balance, where it supports margin again. Goes in the queue to be approved.`,
          ),
        ],
      })
    }

    items.push({
      label: 'Manual trade',
      icon: '\u26a1',
      disabled: a.id !== selected,
      hint: a.id === selected ? undefined : 'select it first',
      items: (['buy', 'sell'] as const).map((way) => ({
        label: way === 'buy' ? 'Buy at the market\u2026' : 'Sell at the market\u2026',
        icon: way === 'buy' ? '\u25b2' : '\u25bc',
        onPick: () =>
          setAsk({
            title: `${way === 'buy' ? 'Buy' : 'Sell'} ${instrument.symbol}`,
            note: `At the market, on account ${a.id} \u2014 ${who}.`,
            fields: [{ key: 'lots', label: 'Volume', initial: '0.01', mode: 'decimal' as const }],
            submitLabel: way === 'buy' ? 'Buy' : 'Sell',
            ...(way === 'sell' ? { tone: 'danger' as const } : {}),
            run: (v) => {
              let volume
              try {
                volume = qty((v['lots'] ?? '').trim())
              } catch {
                return
              }
              if (volume <= 0n) return
              gw.placeOrder({ instrumentId: instrument.id, side: way, kind: 'market', qty: volume })
            },
          }),
      })),
    })

    if (a.owner !== null) {
      items.push({
        label: 'Open a sub-account',
        icon: '\u229e',
        items: [
          {
            label: 'Another demo account',
            onPick: () => {
              if (!window.confirm(`Open another demo account for ${who}?`)) return
              gw.adminOpenAccount(a.owner!, false)
              after(350)
            },
          },
          {
            label: 'A live account',
            disabled: !isAdmin || owner?.kyc !== 'approved',
            hint: owner?.kyc === 'approved' ? undefined : 'needs verifying',
            onPick: () => {
              if (!window.confirm(`Open a LIVE account for ${who}? The money in it will be theirs.`))
                return
              gw.adminOpenAccount(a.owner!, true)
              after(350)
            },
          },
        ],
      })
    }

    // ── The person ──
    if (isAdmin && a.owner !== null) {
      items.push('-')
      items.push({
        label: 'Edit client',
        icon: '\u270e',
        items: [
          {
            label: 'Put them in a book\u2026',
            hint: owner?.managerLogin ?? 'unassigned',
            onPick: () =>
              setAsk({
                title: `Which book holds ${owner?.login ?? 'this client'}?`,
                note: 'A manager sees the accounts of the clients in their book, and no others.',
                fields: [
                  {
                    key: 'manager',
                    label: 'Manager',
                    required: false,
                    choices: [
                      { value: '', label: '\u2014 nobody \u2014' },
                      ...managers.map((m) => ({ value: String(m.id), label: m.login })),
                    ],
                    initial: owner?.manager === null ? '' : String(owner?.manager ?? ''),
                  },
                ],
                submitLabel: 'Assign',
                run: (v) => {
                  const to = (v['manager'] ?? '').trim()
                  gw.adminAssignManager(a.owner!, to === '' ? null : Number(to))
                  after(250)
                },
              }),
          },
          {
            label: 'Reset their password\u2026',
            onPick: () =>
              setAsk({
                title: `New password for ${owner?.login ?? 'this client'}`,
                note: 'Every session they have open is signed out. A password change that leaves old sessions alive has revoked nothing.',
                fields: [
                  {
                    key: 'password',
                    label: 'Password',
                    placeholder: 'at least eight characters',
                    mode: 'text' as const,
                  },
                ],
                submitLabel: 'Set it',
                tone: 'danger' as const,
                run: (v) => {
                  const pw = v['password'] ?? ''
                  if (pw.length >= 8) gw.adminSetPassword(a.owner!, pw)
                },
              }),
          },
          '-',
          owner?.role === 'manager'
            ? {
                label: 'Take the manager role away',
                tone: 'danger' as const,
                onPick: () => {
                  if (
                    window.confirm(
                      `Take the manager role from ${owner.login}? Their clients will be left unassigned.`,
                    )
                  ) {
                    gw.adminSetRole(owner.id, 'client')
                    after(250)
                  }
                },
              }
            : {
                label: 'Make them a manager',
                onPick: () => {
                  if (
                    window.confirm(
                      `Make ${who} a manager? They will see the clients you assign to them, and no others.`,
                    )
                  ) {
                    gw.adminSetRole(a.owner!, 'manager')
                    after(250)
                  }
                },
              },
          // The switches, where the role that needs them is set. A
          // manager's powers start on and are taken away one at a time,
          // because starting a new manager with nothing means somebody
          // has to find eleven switches before the desk can work — and
          // what people do faced with that is turn all eleven on without
          // reading them.
          ...(owner?.role === 'manager'
            ? ([
                '-',
                {
                  label: 'What this desk may do',
                  icon: '\u26bf',
                  hint: `${owner.powers.filter((p) => p.held).length}/${owner.powers.filter((p) => p.possible).length}`,
                  items: owner.powers.map((power) => ({
                    label: power.said,
                    checked: power.held,
                    disabled: !power.possible,
                    // The impossible ones stay on the list, greyed, with
                    // the reason: a power that simply vanished would
                    // leave an operator hunting for it.
                    hint: power.whyNot ?? undefined,
                    onPick: () => {
                      gw.adminSetPower(owner.id, power.key, !power.held)
                      after(250)
                    },
                  })),
                },
              ] as MenuItem[])
            : []),
        ],
      })

      items.push({
        label: 'Identity',
        icon: '\u25a4',
        hint: kycWords(owner?.kyc ?? 'none'),
        badge:
          owner?.kyc === 'approved'
            ? ('ok' as const)
            : owner?.kyc === 'rejected'
              ? ('bad' as const)
              : ('warn' as const),
        items: [
          {
            label: 'Verify this client',
            disabled: owner?.kyc === 'approved',
            onPick: () => {
              gw.adminSetKyc(a.owner!, 'approved')
              after(200)
            },
          },
          {
            label: 'Refuse verification',
            tone: 'danger' as const,
            disabled: owner?.kyc === 'rejected',
            onPick: () => {
              gw.adminSetKyc(a.owner!, 'rejected')
              after(200)
            },
          },
          '-',
          {
            label: 'No documents are collected',
            disabled: true,
            hint: 'not built',
          },
        ],
      })
    }

    // ── The switches ──
    items.push({
      label: 'Settings',
      icon: '\u2699',
      items: [
        {
          label: a.hedging ? 'Net opposing orders off' : 'Allow hedging',
          hint: a.hedging ? 'hedging' : 'netting',
          onPick: () => {
            const kind: AdminActionKind = a.hedging ? 'netting' : 'hedging'
            if (!window.confirm(describeAction(kind, a.id, ''))) return
            gw.adminAction(a.id, kind, '')
            after()
          },
        },
        ...(a.live
          ? isAdmin
            ? [{ ...plain('demo', 'Make it a demo account'), tone: 'danger' as const }]
            : []
          : [
              { ...plain('renew', 'Renew for 30 days'), hint: daysLeft(a) },
              ...(isAdmin ? [plain('live', 'Make it live')] : []),
            ]),
        '-',
        {
          label: 'Show its history',
          onPick: () => {
            gw.selectAccount(a.id)
            setTab('History')
          },
        },
      ],
    })

    // ── This account, and all of them ──
    items.push({ heading: 'This account' })
    items.push({
      chips: [
        a.tradeable
          ? {
              label: 'Lock',
              icon: '\u25a0',
              tone: 'danger' as const,
              onPick: () => {
                if (!window.confirm(describeAction('suspend', a.id, ''))) return
                gw.adminAction(a.id, 'suspend', '')
                after()
              },
            }
          : {
              label: 'Unlock',
              icon: '\u25a1',
              tone: 'ok' as const,
              onPick: () => {
                gw.adminAction(a.id, 'reinstate', '')
                after()
              },
            },
        {
          label: a.id === selected ? 'Acting on it' : 'Act on it',
          icon: '\u25b8',
          tone: 'plain' as const,
          disabled: a.id === selected,
          onPick: () => gw.selectAccount(a.id),
        },
      ],
    })

    if (a.owner !== null && theirs.length > 1) {
      items.push({ heading: `All ${theirs.length} of their accounts` })
      items.push({
        chips: [
          {
            label: 'Lock all',
            icon: '\u25a0',
            tone: 'danger' as const,
            disabled: theirs.every((x) => !x.tradeable),
            onPick: sweep('suspend', 'Lock'),
          },
          {
            label: 'Unlock all',
            icon: '\u25a1',
            tone: 'ok' as const,
            disabled: theirs.every((x) => x.tradeable),
            onPick: sweep('reinstate', 'Unlock'),
          },
        ],
      })
    }

    if (isAdmin) {
      items.push({
        label: 'Put a trade right…',
        icon: '↺',
        onPick: () =>
          setAsk({
            title: `Put a trade right on ${a.id}`,
            note: 'Nothing edits the filled trade. This writes a new, permanent entry with your name and the reason on it, and the client is told.',
            fields: [
              {
                key: 'ticket',
                label: 'Ticket',
                placeholder: 'ticket number',
                mode: 'numeric' as const,
                required: false,
              },
              {
                key: 'reason',
                label: 'Reason',
                choices: [
                  { value: 'badPrice', label: 'Price was not the market’s' },
                  { value: 'dealingError', label: 'Dealing error' },
                  { value: 'feeReturned', label: 'Charge returned' },
                  { value: 'goodwill', label: 'Goodwill' },
                  { value: 'other', label: 'Other' },
                ],
              },
              {
                key: 'how',
                label: 'What to do',
                choices: [
                  { value: 'adjust', label: 'Adjust money by this amount' },
                  { value: 'correctPrice', label: 'Treat it as opened at this price' },
                ],
              },
              { key: 'value', label: 'Amount or price', placeholder: '0.00', mode: 'decimal' as const },
            ],
            submitLabel: 'Record it',
            run: (v) => {
              const ticket = Number((v['ticket'] ?? '').trim() || '0')
              gw.adminAmend(
                a.id,
                ticket,
                (v['how'] ?? 'adjust') as 'adjust' | 'correctPrice',
                (v['reason'] ?? 'other') as AmendReason,
                (v['value'] ?? '').trim(),
              )
              window.setTimeout(() => {
                gw.adminOverview()
                gw.adminAudit()
              }, 200)
            },
          }),
      })
    }

    // ── What you cannot take back ──
    const open = a.id === selected ? gw.positions : []
    items.push('-')
    items.push({
      label: 'Close every position',
      icon: '\u2715',
      tone: 'danger' as const,
      disabled: open.length === 0,
      hint: a.id === selected ? (open.length > 0 ? `${open.length} open` : 'none open') : 'select it first',
      onPick: () => {
        if (!window.confirm(`Close all ${open.length} positions on account ${a.id} \u2014 ${who}? This deals at the market.`))
          return
        for (const p of open) gw.closePosition(p.ticket)
      },
    })

    return items
  }

  /** What a right-click offers on an open position. */
  const positionMenu = (p: GatewayPosition): readonly MenuItem[] => {
    const digits = digitsOf(instruments, p.instrumentId)
    const symbol = symbolOf(instruments, p.instrumentId)
    const tail: readonly MenuItem[] = [
      '-',
      {
        label: 'Open chart',
        icon: '\u25a5',
        onPick: () => openChart(p.instrumentId),
      },
      {
        label: 'Copy ticket',
        icon: '\u29c9',
        // The number somebody reads out on the phone to support. The
        // clipboard can be refused; the ticket is on screen either way.
        onPick: () =>
          void navigator.clipboard?.writeText(String(p.ticket)).catch(() => {}),
      },
    ]
    return [
      {
        label: 'Close this position',
        tone: 'danger',
        onPick: () => {
          if (window.confirm(`Close ticket ${p.ticket} on account ${selected}?`))
            gw.closePosition(p.ticket)
        },
      },
      {
        label: 'Close part of it' + DOTS,
        hint: showQtyAsLots(p.qty, contractOf(instruments, p.instrumentId), 3),
        onPick: () => {
          const contract = contractOf(instruments, p.instrumentId)
          const whole = showQtyAsLots(p.qty, contract, 3)
          setAsk({
            title: `Close part of ticket ${p.ticket}`,
            subject: `${symbol}, ${whole} open at ${showPrice(p.openPrice, digits)}`,
            icon: '\u2702',
            note: 'What is left stays open at the price it got, which is the point: closing it all and re-opening pays the spread twice and gives up the entry. Type less than the volume shown.',
            fields: [
              {
                key: 'lots',
                label: 'Volume to close',
                mode: 'decimal' as const,
                // Pre-filled with the whole position, because a default
                // that gets refused is worse than one that does nothing
                // surprising: half of an odd size can land off the step
                // grid, and the engine would turn it away.
                initial: whole,
              },
            ],
            submitLabel: 'Close that much',
            tone: 'danger' as const,
            run: ({ lots }) => {
              let typed
              try {
                typed = qty((lots ?? '').trim())
              } catch {
                return
              }
              if (typed <= 0n) return
              const size = asQty(typed, contract)
              if (
                !window.confirm(
                  `Close ${lots} of ticket ${p.ticket}? ${
                    size >= p.qty
                      ? 'That is the whole position.'
                      : `${showQtyAsLots((p.qty - size) as Qty, contract, 3)} stays open at ${showPrice(p.openPrice, digits)}.`
                  }`,
                )
              )
                return
              // Zero would mean "all of it", and a size at or above the
              // position means the same, so either way the engine closes
              // the lot. Sent as a quantity, which is what the wire deals
              // in.
              gw.closePosition(p.ticket, showQty(size))
            },
          })
        },
      },
      '-',
      {
        label: 'Set a stop loss\u2026',
        hint: p.stopLoss ? showPrice(p.stopLoss, digits) : 'none',
        onPick: () =>
          setAsk({
            title: `Stop loss on ticket ${p.ticket}`,
            note: `${symbol}, opened at ${showPrice(p.openPrice, digits)}. Leave it empty to remove the stop.`,
            fields: [
              {
                key: 'level',
                label: 'Price',
                mode: 'decimal' as const,
                required: false,
                initial: p.stopLoss ? showPrice(p.stopLoss, digits) : '',
              },
            ],
            submitLabel: 'Set it',
            run: (v) => {
              const text = (v['level'] ?? '').trim()
              gw.setProtection(p.ticket, text === '' ? null : parsePrice(text), p.takeProfit)
            },
          }),
      },
      {
        label: 'Set a take profit\u2026',
        hint: p.takeProfit ? showPrice(p.takeProfit, digits) : 'none',
        onPick: () =>
          setAsk({
            title: `Take profit on ticket ${p.ticket}`,
            note: `${symbol}, opened at ${showPrice(p.openPrice, digits)}. Leave it empty to remove it.`,
            fields: [
              {
                key: 'level',
                label: 'Price',
                mode: 'decimal' as const,
                required: false,
                initial: p.takeProfit ? showPrice(p.takeProfit, digits) : '',
              },
            ],
            submitLabel: 'Set it',
            run: (v) => {
              const text = (v['level'] ?? '').trim()
              gw.setProtection(p.ticket, p.stopLoss, text === '' ? null : parsePrice(text))
            },
          }),
      },
      ...tail,
    ]
  }

  /** What a right-click offers on a price. */
  const priceMenu = (inst: Instrument): readonly MenuItem[] => {
    const q = gw.quotes.get(inst.id)
    const order = (way: 'buy' | 'sell'): MenuItem => ({
      label: way === 'buy' ? 'Buy\u2026' : 'Sell\u2026',
      disabled: !account || !q,
      hint: account
        ? q
          ? showPrice(way === 'buy' ? q.ask : q.bid, inst.displayDigits)
          : 'no price'
        : 'no account',
      onPick: () =>
        setAsk({
          title: `${way === 'buy' ? 'Buy' : 'Sell'} ${inst.symbol}`,
          note: `At the market, on account ${account?.id} \u2014 ${account?.ownerLogin ?? 'unclaimed'}.`,
          fields: [{ key: 'lots', label: 'Volume', initial: '0.01', mode: 'decimal' as const }],
          submitLabel: way === 'buy' ? 'Buy' : 'Sell',
          ...(way === 'sell' ? { tone: 'danger' as const } : {}),
          run: (v) => {
            let volume
            try {
              volume = qty((v['lots'] ?? '').trim())
            } catch {
              return
            }
            if (volume <= 0n) return
            gw.placeOrder({ instrumentId: inst.id, side: way, kind: 'market', qty: volume })
          },
        }),
    })
    return [
      order('buy'),
      order('sell'),
      '-',
      { label: 'Open chart', icon: '\u25a5', onPick: () => openChart(inst.id) },
      {
        label: 'Copy symbol',
        icon: '\u29c9',
        // The clipboard can be refused. The symbol is on screen either
        // way, so this is a convenience failing rather than the thing
        // failing, and it says nothing when it does.
        onPick: () => void navigator.clipboard?.writeText(inst.symbol).catch(() => {}),
      },
    ]
  }

  /** What the New menu offers: the things that make a person or a chart. */
  const newMenu = (): readonly MenuItem[] => {
    const items: MenuItem[] = []
    if (isAdmin) {
      items.push(
        { label: 'Client' + DOTS, icon: '\u25cb', onPick: () => setAsk(newPersonDialog('client')) },
        { label: 'Manager' + DOTS, icon: '\u25c9', onPick: () => setAsk(newPersonDialog('manager')) },
        '-',
      )
    }
    items.push({
      label: `Chart of ${instrument.symbol}`,
      icon: '\u25a5',
      onPick: () => openChart(instrument.id),
    })
    return items
  }

  /** What the Charts menu offers: the timeframe, and what is drawn on it. */
  const chartsMenu = (): readonly MenuItem[] => [
    {
      label: 'Timeframe',
      icon: '\u23f1',
      hint: TF_LABEL[interval],
      items: (Object.keys(TF_LABEL) as (keyof typeof TF_LABEL)[]).map((tf) => ({
        label: TF_LABEL[tf],
        checked: tf === interval,
        onPick: () => setInterval(tf),
      })),
    },
    '-',
    ...charts.map((chart) => {
      const inst = instruments.find((i) => i.id === chart.id)
      return {
        label: inst?.symbol ?? String(chart.id),
        checked: chart.id === symbol,
        hint: TF_LABEL[chart.interval],
        onPick: () => setSymbol(chart.id),
      }
    }),
  ]

  /** What the Reports menu offers: the figures somebody is asked for. */
  const reportsMenu = (): readonly MenuItem[] => {
    const items: MenuItem[] = [
      {
        label: 'Statement for this account',
        icon: '\u2630',
        onPick: () => {
          gw.requestStatement()
          setTab('History')
        },
      },
      {
        label: 'Money waiting on the desk',
        icon: '\u25c9',
        hint: String(gw.deposits.filter((d) => d.stage === 'pending').length),
        onPick: () => {
          gw.adminDepositRequests()
          setTab('Requests')
        },
      },
    ]
    if (isAdmin) {
      items.push(
        '-',
        {
          label: 'Audit trail',
          icon: '\u25a4',
          onPick: () => {
            gw.adminAudit()
            setTab('Audit')
          },
        },
        {
          label: 'Trial balance' + DOTS,
          icon: '\u2696',
          onPick: () => {
            gw.adminBooks()
            setSheet('books')
          },
        },
        {
          label: 'Pricing' + DOTS,
          icon: '\u0025',
          onPick: () => {
            gw.markups()
            setSheet('pricing')
          },
        },
      )
    }
    return items
  }

  /**
   * What a right-click offers on nothing in particular.
   *
   * The background menu is where the things that have no row live: making a
   * person, and arranging the screen. A new client is not an action *on* an
   * existing one, so it does not belong on any row.
   */
  const backgroundMenu = (): readonly MenuItem[] => {
    const items: MenuItem[] = []
    if (isAdmin) {
      items.push(
        { label: 'New client\u2026', onPick: () => setAsk(newPersonDialog('client')) },
        { label: 'New manager\u2026', onPick: () => setAsk(newPersonDialog('manager')) },
        '-',
        {
          label: 'Payment methods\u2026',
          onPick: () => {
            gw.paymentMethods()
            setSheet('payments')
          },
        },
        {
          label: 'Pricing…',
          hint: 'markup and commission',
          onPick: () => {
            gw.markups()
            setSheet('pricing')
          },
        },
        {
          label: 'Trial balance…',
          onPick: () => {
            gw.adminBooks()
            setSheet('books')
          },
        },
      )
    }
    items.push(
      {
        label: 'Two-factor' + DOTS,
        icon: gw.session?.secondFactor ? '\u25c9' : '\u25cb',
        hint: gw.session?.secondFactor ? 'on' : 'off',
        onPick: () => setSheet('second'),
      },
      { label: 'Refresh the book', onPick: () => gw.adminOverview() },
      '-',
      ...PANES.map(([pane, label]) => ({
        label,
        checked: layout.open[pane],
        onPick: () => show(pane),
      })),
    )
    return items
  }

  /**
   * Creating a person.
   *
   * A manager is made in one gesture rather than created-then-promoted: two
   * steps to make one thing is two chances to leave it half done.
   */
  const newPersonDialog = (kind: 'client' | 'manager') => ({
    title: kind === 'client' ? 'New client' : 'New manager',
    note:
      kind === 'client'
        ? 'The email address is the login, and only one person may have it. Their demo account opens the first time they sign in; a live one needs their identity verified.'
        : 'A manager sees this same screen, over the clients you assign to them. They may work those accounts but not move money, make one live, or decide identity.',
    fields: [
      {
        key: 'login',
        label: 'Email address',
        placeholder: 'name@example.com',
        mode: 'text' as const,
      },
      {
        key: 'password',
        label: 'Password',
        placeholder: 'at least eight characters',
        mode: 'text' as const,
      },
    ],
    submitLabel: kind === 'client' ? 'Create client' : 'Create manager',
    run: (v: Record<string, string>) => {
      const login = (v['login'] ?? '').trim()
      const password = v['password'] ?? ''
      if (!login.includes('@') || password.length < 8) return
      gw.adminCreateUser(login, password)
      // The role is set once the login exists and the book has come back
      // with its id; creating and promoting in one gesture is the point.
      if (kind === 'manager') setPromote(login.toLowerCase())
      window.setTimeout(() => gw.adminOverview(), 400)
    },
  })

  /** Shows a symbol, adding a tab for it when it has none. */
  const openChart = (id: number) => {
    setCharts((open) =>
      open.some((c) => c.id === id) ? open : [...open, { id, interval: '5m' }],
    )
    setSymbol(id)
    if (!layout.open.chart) show('chart')
  }

  /** The timeframe of the chart in front. */
  const interval = charts.find((c) => c.id === symbol)?.interval ?? '5m'
  const setInterval = (next: Interval) =>
    setCharts((open) => open.map((c) => (c.id === symbol ? { ...c, interval: next } : c)))


  const positionColumns: readonly Column<GatewayPosition>[] = [
    {
      key: 'tick',
      header: '',
      width: 26,
      render: (p) => (
        <input
          type="checkbox"
          className="row-tick"
          checked={ticked.has(p.ticket)}
          aria-label={`Select ticket ${p.ticket}`}
          onClick={(e) => e.stopPropagation()}
          onChange={() => tick(p.ticket)}
        />
      ),
    },
    { key: 'ticket', header: 'Ticket', width: 72, render: (p) => <span className="mono dim">{p.ticket}</span> },
    {
      key: 'sym',
      header: 'Symbol',
      render: (p) => <span className="sym">{symbolOf(instruments, p.instrumentId)}</span>,
    },
    {
      key: 'side',
      header: 'Type',
      width: 64,
      render: (p) => (
        <span className="pos-side">
          <span className={`pos-dot pos-${p.side}`} aria-hidden="true" />
          {p.side === 'buy' ? 'Buy' : 'Sell'}
        </span>
      ),
    },
    {
      key: 'vol',
      header: 'Volume',
      align: 'right',
      render: (p) => showQtyAsLots(p.qty, contractOf(instruments, p.instrumentId), 3),
    },
    {
      key: 'open',
      header: 'Open',
      align: 'right',
      render: (p) => <span className="mono">{showPrice(p.openPrice, digitsOf(instruments, p.instrumentId))}</span>,
    },
    {
      key: 'now',
      header: 'Now',
      align: 'right',
      render: (p) =>
        p.current ? (
          <span className="mono">{showPrice(p.current, digitsOf(instruments, p.instrumentId))}</span>
        ) : (
          <span className="dim">—</span>
        ),
    },
    {
      key: 'sl',
      header: 'S/L',
      align: 'right',
      render: (p) => (
        <LevelCell
          position={p}
          level="stopLoss"
          digits={digitsOf(instruments, p.instrumentId)}
          onSet={(value) => gw.setProtection(p.ticket, value, p.takeProfit)}
        />
      ),
    },
    {
      key: 'tp',
      header: 'T/P',
      align: 'right',
      render: (p) => (
        <LevelCell
          position={p}
          level="takeProfit"
          digits={digitsOf(instruments, p.instrumentId)}
          onSet={(value) => gw.setProtection(p.ticket, p.stopLoss, value)}
        />
      ),
    },
    {
      key: 'swap',
      header: 'Interest',
      align: 'right',
      render: (p) => (p.swap === 0n ? <span className="dim">0.00</span> : showSigned(p.swap)),
    },
    {
      key: 'pnl',
      header: 'P&L',
      align: 'right',
      render: (p) => <span className={p.profit < 0n ? 'loss' : 'profit'}>{showSigned(p.profit)}</span>,
    },
    {
      key: 'close',
      header: '',
      width: 64,
      render: (p) => (
        <button
          type="button"
          className="mgr-mini"
          onClick={() => {
            if (window.confirm(`Close ticket ${p.ticket} on account ${selected}?`)) {
              gw.closePosition(p.ticket)
            }
          }}
        >
          Close
        </button>
      ),
    },
  ]

  return (
    <div className="manager">
      {/* ── Command bar ── */}
      <header className="mgr-bar">
        <span className="mgr-mark">F</span>
        <span className="brand-name">FERARO</span>
        <span className="mgr-pill">{isAdmin ? 'BACK OFFICE' : 'MANAGER'}</span>

        {/* The menu bar, as Cubex has it. Everything here was already in
            the product and reachable only by right-clicking, which is a
            fine shortcut and a poor way to find out something exists. */}
        <nav className="mgr-menubar">
          <button
            type="button"
            className="mgr-menu-top"
            onClick={(e) =>
              setMenu({
                x: e.currentTarget.getBoundingClientRect().left,
                y: e.currentTarget.getBoundingClientRect().bottom,
                subject: { title: 'New' },
                items: newMenu(),
              })
            }
          >
            New
          </button>
          <button
            type="button"
            className="mgr-menu-top"
            onClick={(e) =>
              setMenu({
                x: e.currentTarget.getBoundingClientRect().left,
                y: e.currentTarget.getBoundingClientRect().bottom,
                subject: { title: 'Charts' },
                items: chartsMenu(),
              })
            }
          >
            Charts
          </button>
          <button
            type="button"
            className="mgr-menu-top"
            onClick={(e) =>
              setMenu({
                x: e.currentTarget.getBoundingClientRect().left,
                y: e.currentTarget.getBoundingClientRect().bottom,
                subject: { title: 'Reports' },
                items: reportsMenu(),
              })
            }
          >
            Reports
          </button>
        </nav>

        <div className="grow" />
        <span className="cap">{gw.session?.login}</span>

        <div className="mgr-view">
          <button
            type="button"
            className="mgr-plain"
            aria-expanded={viewOpen}
            onClick={() => setViewOpen((o) => !o)}
          >
            View
          </button>
          {viewOpen ? (
            <>
              <div className="mgr-view-catch" onPointerDown={() => setViewOpen(false)} />
              <div className="mgr-view-menu" role="menu">
                {PANES.map(([pane, label]) => (
                  <label key={pane} className="mgr-view-row">
                    <input
                      type="checkbox"
                      checked={layout.open[pane]}
                      onChange={() => show(pane)}
                    />
                    <span>{label}</span>
                  </label>
                ))}
                <div className="ctx-rule" />
                <label className="mgr-view-row">
                  <input
                    type="checkbox"
                    checked={!quiet}
                    onChange={() => {
                      const off = !quiet
                      setMuted(off)
                      setQuiet(off)
                      if (!off) {
                        wake()
                        play('signIn')
                      }
                    }}
                  />
                  <span>Sound</span>
                </label>
                <div className="ctx-rule" />
                <button
                  type="button"
                  className="mgr-view-reset"
                  onClick={() => {
                    setLayout(DEFAULT_LAYOUT)
                    setViewOpen(false)
                  }}
                >
                  Reset the layout
                </button>
              </div>
            </>
          ) : null}
        </div>

        <button type="button" className="mgr-plain" onClick={gw.signOut}>
          Sign out
        </button>
      </header>

      {gw.adminResult ? (
        <p className={`mgr-result${gw.adminResult.done ? '' : ' bad'}`} role="status">
          {gw.adminResult.detail}
        </p>
      ) : null}

      <div
        className="mgr-body"
        onPointerDownCapture={wake}
        onContextMenu={(e) => {
          // Only the background: a row's own handler has already stopped
          // this, so reaching here means nothing was under the pointer.
          e.preventDefault()
          setMenu({
            x: e.clientX,
            y: e.clientY,
            subject: {
              title: isAdmin ? 'Back office' : 'Manager',
              subtitle: gw.session?.login ?? '',
              initial: 'F',
            },
            items: backgroundMenu(),
          })
        }}
      >
        <div className="mgr-upper">
        {/* ── Clients, left ── */}
        {layout.open.navigator ? (
        <aside className="mgr-clients" style={{ width: layout.nav }}>
          <div className="panel-head">
            <span className="cap">Navigator</span>
            <button
              type="button"
              className="pane-shut"
              aria-label="Hide the navigator"
              title="Hide the navigator"
              onClick={() => show('navigator')}
            >
              ×
            </button>
          </div>

          {/* Live and demo as two lists rather than one, as Cubex has it.
              They are different populations and a desk works one at a
              time: a demo account in a list of live ones is a row
              somebody has to re-read to be sure it is not real. */}
          <div className="mgr-side-tabs">
            {(['live', 'demo'] as const).map((which) => (
              <button
                key={which}
                type="button"
                className={side === which ? 'on' : undefined}
                onClick={() => setSide(which)}
              >
                {which === 'live' ? 'LIVE' : 'DEMO'}
                <span className="mgr-side-count">
                  {matches.filter((a) => (which === 'live' ? a.live : !a.live)).length}
                </span>
              </button>
            ))}
          </div>

          <label className="mgr-find-row">
            <span className="cap">Account</span>
            <input
              className="mgr-find"
              value={find}
              onChange={(e) => setFind(e.target.value)}
              placeholder="number or email"
              spellCheck={false}
              autoCapitalize="off"
              aria-label="Find an account"
            />
          </label>

          <div className="mgr-tree" role="tree">
              <Branch
                label="Feraro"
                depth={0}
                kind="root"
                shut={shut.has('root')}
                onFold={() => fold('root')}
              />
              {shut.has('root') ? null : (
                <>
                  {favourites.length > 0 ? (
                    <>
                      <Branch
                        label="Favourites"
                        depth={1}
                        kind="star"
                        count={favourites.length}
                        shut={shut.has('fav')}
                        onFold={() => fold('fav')}
                      />
                      {shut.has('fav')
                        ? null
                        : favourites.map((row) => (
                            <AccountRow
                              key={`fav-${row.account.id}`}
                              row={row}
                              depth={2}
                              selected={selected}
                              showManager={isAdmin}
                              favourite={isFavourite(row.account.id)}
                              onPick={() => gw.selectAccount(row.account.id)}
                              onStar={() => star(row.account.id)}
                              onMenu={(x, y) =>
                                setMenu({
                                  x,
                                  y,
                                  subject: subjectOf(row.account),
                                  items: accountMenu(row.account),
                                })
                              }
                            />
                          ))}
                    </>
                  ) : null}

                  <Branch
                    label="Accounts"
                    depth={1}
                    kind="folder"
                    count={matches.length}
                    shut={shut.has('accounts')}
                    onFold={() => fold('accounts')}
                  />
                  {shut.has('accounts')
                    ? null
                    : tree.map((group) => (
                        <Fragment key={group.manager}>
                          <Branch
                            label={group.manager === UNASSIGNED ? 'Unassigned' : group.manager}
                            depth={2}
                            kind={group.manager === UNASSIGNED ? 'loose' : 'manager'}
                            count={group.live.length + group.demo.length}
                            shut={shut.has(group.manager)}
                            onFold={() => fold(group.manager)}
                          />
                          {shut.has(group.manager)
                            ? null
                            : (
                                [
                                  ['Live', group.live],
                                  ['Demo', group.demo],
                                ] as const
                              ).map(([kind, rows]) =>
                                rows.length === 0 ? null : (
                                  <Fragment key={kind}>
                                    <div className="mgr-leafhead" style={{ paddingLeft: 12 + 3 * 13 }}>
                                      {kind}
                                    </div>
                                    {rows.map((row) => (
                                      <AccountRow
                                        key={row.account.id}
                                        row={row}
                                        depth={4}
                                        selected={selected}
                                        showManager={false}
                                        favourite={isFavourite(row.account.id)}
                                        onPick={() => gw.selectAccount(row.account.id)}
                                        onStar={() => star(row.account.id)}
                                        onMenu={(x, y) =>
                                          setMenu({
                                            x,
                                            y,
                                            subject: subjectOf(row.account),
                                            items: accountMenu(row.account),
                                          })
                                        }
                                      />
                                    ))}
                                  </Fragment>
                                ),
                              )}
                        </Fragment>
                      ))}

                  {isAdmin ? (
                    <>
                      <Branch
                        label="Managers"
                        depth={1}
                        kind="people"
                        count={managers.length}
                        shut={shut.has('managers')}
                        onFold={() => fold('managers')}
                      />
                      {shut.has('managers')
                        ? null
                        : managers.map((m) => (
                            <button
                              key={m.id}
                              type="button"
                              className="mgr-acct"
                              style={{ paddingLeft: 12 + 2 * 13 }}
                              onClick={() => setFind(m.login)}
                              title={`Show only ${m.login}'s book`}
                            >
                              <span className="mgr-acct-line">
                                <span className="mgr-dot dot-ok" aria-hidden="true" />
                                <span className="mgr-acct-who">{m.login}</span>
                                <span className="mgr-kyc">
                                  {gw.users.filter((u) => u.manager === m.id).length}
                                </span>
                              </span>
                            </button>
                          ))}
                    </>
                  ) : null}
                </>
              )}

              {matches.length === 0 ? (
                <p className="mgr-empty dim">
                  {find.trim() ? `Nothing matches \u201c${find.trim()}\u201d.` : 'No accounts yet.'}
                </p>
              ) : null}
            </div>
        </aside>
        ) : null}

        {layout.open.navigator && (layout.open.chart || layout.open.watch) ? (
          <Splitter
            direction="vertical"
            label="Navigator width"
            onDrag={(d) => resize('nav', d)}
            onNudge={(d) => resize('nav', d)}
            onDoubleClick={() => show('navigator')}
          />
        ) : null}

        {/* ── Chart, centre ── */}
        {layout.open.chart ? (
        <section className="mgr-chart">
          <div className="mgr-symtabs mgr-head-tabs">
            {charts.map((chart) => {
              const inst = instruments.find((i) => i.id === chart.id)
              if (!inst) return null
              return (
                <span key={chart.id} className={`symtab${chart.id === symbol ? ' on' : ''}`}>
                  <button
                    type="button"
                    className="symtab-pick"
                    onClick={() => setSymbol(chart.id)}
                  >
                    {inst.symbol}-{TF_LABEL[chart.interval]}
                  </button>
                  {charts.length > 1 ? (
                    <button
                      type="button"
                      className="symtab-shut"
                      aria-label={`Close ${inst.symbol}`}
                      onClick={() => {
                        const rest = charts.filter((c) => c.id !== chart.id)
                        setCharts(rest)
                        if (chart.id === symbol) {
                          setSymbol(rest[0]?.id ?? DEFAULT_INSTRUMENT.id)
                        }
                      }}
                    >
                      ×
                    </button>
                  ) : null}
                </span>
              )
            })}
            {/* Cubex's "+ Add" rather than a sentence. A tab strip whose
                last item is an instruction is a strip that has given up on
                being a tab strip — the affordance should say what it does
                and then do it. */}
            <button
              type="button"
              className="symtab-add"
              title="Open another chart from the market watch"
              onClick={() => openChart(instrument.id)}
            >
              + Add
            </button>
          </div>
          <div className="mgr-chart-area">
            <ChartPane
              gw={gw}
              instrument={instrument}
              interval={interval}
              onInterval={setInterval}
              last={quote?.bid}
              lines={[
                ...positionLines(gw.positions, instrument),
                ...restingLines(gw.pending, instrument),
                ...aimLines(aiming, instrument),
              ]}
              overlay={
                <DealTicket
                  instrument={instrument}
                  quote={quote}
                  account={account ? account.id : null}
                  refuse={dealRefusal(account, gw.connected)}
                  onPlace={deal}
                  onAim={setAiming}
                />
              }
            />
          </div>
        </section>
        ) : null}

        {layout.open.watch && (layout.open.chart || layout.open.navigator) ? (
          <Splitter
            direction="vertical"
            label="Market watch width"
            onDrag={(d) => resize('watch', -d)}
            onNudge={(d) => resize('watch', -d)}
            onDoubleClick={() => show('watch')}
          />
        ) : null}

        {/* ── Market watch, right ── */}
        {layout.open.watch ? (
        <aside className="mgr-watch" style={{ width: layout.watch }}>
          <div className="panel-head">
            <span className="cap">Market Watch</span>
            <span className="panel-meta">{instruments.length}</span>
            <button
              type="button"
              className="pane-shut"
              aria-label="Hide the market watch"
              title="Hide the market watch"
              onClick={() => show('watch')}
            >
              ×
            </button>
          </div>
          <div
            className="mgr-watch-body"
            onContextMenu={(e) => {
              const row = (e.target as HTMLElement).closest('tr[data-instrument]')
              const id = Number(row?.getAttribute('data-instrument') ?? NaN)
              const inst = instruments.find((i) => i.id === id)
              if (!inst) return
              e.preventDefault()
              e.stopPropagation()
              setMenu({
                x: e.clientX,
                y: e.clientY,
                subject: {
                  title: inst.symbol,
                  subtitle: `${inst.base} / ${inst.quote}`,
                  initial: inst.symbol.slice(0, 1),
                },
                items: priceMenu(inst),
              })
            }}
          >
            {watchTab === 'Market Watch' ? (
              <MarketWatch
                instruments={instruments}
                quotes={gw.quotes}
                selected={instrument.id}
                onSelect={openChart}
              />
            ) : (
              <Ticks instrument={instrument} quote={quote} />
            )}
          </div>

          <div className="mgr-foot-tabs">
            {(['Market Watch', 'Ticks'] as const).map((t) => (
              <button
                key={t}
                type="button"
                className={t === watchTab ? 'on' : undefined}
                onClick={() => setWatchTab(t)}
              >
                {t}
              </button>
            ))}
          </div>
        </aside>
        ) : null}
        </div>

        {/* ── The selected account's figures ── */}
        <div className="mgr-status">
          {account ? (
            <>
              <Figure label="Balance" value={showMoney(account.balance)} strip />
              <Figure
                label="Credit"
                value={showMoney(account.credit)}
                tone={account.credit > 0n ? 'var(--credit)' : undefined}
                strip
              />
              <Figure
                label="Flt P/L"
                value={showSigned(openProfit)}
                tone={openProfit < 0n ? 'var(--loss)' : 'var(--profit)'}
                strip
              />
              <Figure label="Equity" value={showMoney(account.equity)} strip />
              <Figure label="Used margin" value={showMoney(account.usedMargin)} strip />
              <Figure
                label="Free margin"
                value={gw.account ? showMoney(gw.account.freeMargin) : '—'}
                strip
              />
              <Figure
                label="Margin level"
                value={showMarginLevel(account.marginLevelBp)}
                tone={levelTone(account.marginLevelBp) ? 'var(--loss)' : undefined}
                strip
              />
              <span className="mgr-sep">|</span>
              <span className="mono">
                {account.id} · {account.ownerLogin ?? 'unclaimed'} ·{' '}
                {account.live ? 'live' : daysLeft(account)}
              </span>
            </>
          ) : (
            <span className="warn">No account selected</span>
          )}
          <div className="grow" />
          <span className="mono dim">
            {gw.quotes.size}/{instruments.length} priced
          </span>
        </div>

        {layout.open.toolbox && (layout.open.chart || layout.open.navigator || layout.open.watch) ? (
          <Splitter
            direction="horizontal"
            label="Toolbox height"
            onDrag={(d) => resize('toolbox', -d)}
            onNudge={(d) => resize('toolbox', -d)}
            onDoubleClick={() => show('toolbox')}
          />
        ) : null}

        {/* ── Toolbox, bottom ── */}
        {layout.open.toolbox ? (
        <section className="mgr-toolbox" style={{ height: layout.toolbox }}>
          {tab === 'Trade' ? (
            <div className="mgr-bulk">
              <label className="mgr-bulk-all">
                <input
                  type="checkbox"
                  checked={gw.positions.length > 0 && chosen.length === gw.positions.length}
                  ref={(el) => {
                    // Part of a selection is neither on nor off, and the
                    // box should say so rather than pick one.
                    if (el) el.indeterminate = chosen.length > 0 && chosen.length < gw.positions.length
                  }}
                  disabled={gw.positions.length === 0}
                  aria-label="Select every position"
                  onChange={() =>
                    setTicked(
                      chosen.length === gw.positions.length
                        ? new Set()
                        : new Set(gw.positions.map((p) => p.ticket)),
                    )
                  }
                />
                <span>{chosen.length > 0 ? `${chosen.length} selected` : 'Select all'}</span>
              </label>

              <button
                type="button"
                className="mgr-bulk-go"
                disabled={chosen.length === 0}
                onClick={closeChosen}
              >
                Close selected positions
              </button>
              <span className="dim">/</span>
              <button
                type="button"
                className="mgr-bulk-go"
                disabled={chosen.length === 0 || !account?.hedging}
                title={
                  account?.hedging
                    ? 'Opens the opposite position beside each one, leaving both open'
                    : 'This account nets opposing orders off, so a hedge would simply close the position. Allow hedging on it first.'
                }
                onClick={hedgeChosen}
              >
                Hedge selected positions
              </button>
              <div className="grow" />
              <button
                type="button"
                className="mgr-bulk-go"
                disabled={gw.pending.length === 0}
                onClick={cancelResting}
              >
                Cancel resting orders
                {gw.pending.length > 0 ? ` (${gw.pending.length})` : ''}
              </button>
            </div>
          ) : null}

          <div
            className="mgr-pane"
            onContextMenu={(e) => {
              const row = (e.target as HTMLElement).closest('tr[data-ticket]')
              const ticket = Number(row?.getAttribute('data-ticket') ?? NaN)
              const held = gw.positions.find((p) => p.ticket === ticket)
              if (!held) return
              e.preventDefault()
              e.stopPropagation()
              setMenu({
                x: e.clientX,
                y: e.clientY,
                subject: {
                  title: `${symbolOf(instruments, held.instrumentId)} \u00b7 ticket ${held.ticket}`,
                  subtitle: `${showQtyAsLots(held.qty, contractOf(instruments, held.instrumentId), 3)} \u00b7 ${showSigned(held.profit)}`,
                  badge: held.side === 'buy' ? 'BUY' : 'SELL',
                  badgeTone: held.side === 'buy' ? 'ok' : 'bad',
                  initial: held.side === 'buy' ? 'B' : 'S',
                },
                items: positionMenu(held),
              })
            }}
          >
            {tab === 'Trade' ? (
              <DataTable
                columns={positionColumns}
                rows={gw.positions}
                rowKey={(p) => p.ticket}
                rowData={(p) => ({ 'data-ticket': p.ticket })}
                totalsLabel={`${gw.positions.length} open`}
                totals={{
                  swap: <span className="mono">{showSigned(openSwap)}</span>,
                  pnl: (
                    <span className={`mono ${openPrice < 0n ? 'loss' : 'profit'}`}>
                      {showSigned(openPrice)}
                    </span>
                  ),
                }}
                empty={`Account ${selected} holds nothing.`}
              />
            ) : null}

            {tab === 'History' ? (
              <div className="table-wrap">
                <table className="table">
                  <thead>
                    <tr>
                      <th style={{ width: 70 }}>Seq</th>
                      <th style={{ width: 150 }}>When</th>
                      <th style={{ width: 150 }}>What</th>
                      <th style={{ width: 80 }}>Ticket</th>
                      <th className="num" style={{ width: 130 }}>
                        Amount
                      </th>
                      <th className="num" style={{ width: 140 }}>
                        Balance
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {gw.statement.map((line) => (
                      <tr key={line.seq}>
                        <td className="mono dim">{line.seq}</td>
                        <td className="mono dim">{when(line.at)}</td>
                        <td>{statementWords(line.kind)}</td>
                        <td className="mono dim">{line.ticket ?? '\u2014'}</td>
                        <td className={`num mono${line.amount < 0n ? ' loss' : ' profit'}`}>
                          {showSigned(line.amount)}
                        </td>
                        <td className="num mono">{showMoney(line.balance)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
                {gw.statement.length === 0 ? (
                  <p className="table-empty dim">
                    Nothing has happened on account {selected} yet.
                  </p>
                ) : null}
              </div>
            ) : null}

            {tab === 'Net Trade' ? (
              <div className="table-wrap">
                <table className="table">
                  <thead>
                    <tr>
                      <th style={{ width: 110 }}>Symbol</th>
                      <th className="num" style={{ width: 100 }}>
                        Long
                      </th>
                      <th className="num" style={{ width: 100 }}>
                        Short
                      </th>
                      <th className="num" style={{ width: 110 }}>
                        Net
                      </th>
                      <th style={{ width: 70 }}>Way</th>
                      <th className="num" style={{ width: 120 }}>
                        Interest
                      </th>
                      <th className="num">P&amp;L</th>
                    </tr>
                  </thead>
                  <tbody>
                    {netTrade.map((row) => {
                      const contract = contractOf(instruments, row.instrumentId)
                      return (
                        <tr key={row.instrumentId}>
                          <td className="sym">{symbolOf(instruments, row.instrumentId)}</td>
                          <td className="num mono dim">
                            {row.long === 0n ? '\u2014' : showQtyAsLots(row.long, contract, 3)}
                          </td>
                          <td className="num mono dim">
                            {row.short === 0n ? '\u2014' : showQtyAsLots(row.short, contract, 3)}
                          </td>
                          <td className="num mono">
                            {showQtyAsLots((row.net < 0n ? -row.net : row.net) as Qty, contract, 3)}
                          </td>
                          <td>
                            {row.net === 0n ? (
                              <span className="dim">flat</span>
                            ) : (
                              <span className="pos-side">
                                <span
                                  className={`pos-dot pos-${row.net > 0n ? 'buy' : 'sell'}`}
                                  aria-hidden="true"
                                />
                                {row.net > 0n ? 'Long' : 'Short'}
                              </span>
                            )}
                          </td>
                          <td className="num mono dim">{showSigned(row.swap)}</td>
                          <td className={`num mono ${row.profit < 0n ? 'loss' : 'profit'}`}>
                            {showSigned(row.profit)}
                          </td>
                        </tr>
                      )
                    })}
                  </tbody>
                  {netTrade.length > 0 ? (
                    <tfoot>
                      <tr className="table-total">
                        <td>{netTrade.length} symbols</td>
                        <td colSpan={4} />
                        <td className="num mono">{showSigned(openSwap)}</td>
                        <td className={`num mono ${openPrice < 0n ? 'loss' : 'profit'}`}>
                          {showSigned(openPrice)}
                        </td>
                      </tr>
                    </tfoot>
                  ) : null}
                </table>
                {netTrade.length === 0 ? (
                  <p className="table-empty dim">Account {selected} is flat.</p>
                ) : null}
              </div>
            ) : null}

            {tab === 'Requests' ? (
              <RequestQueue
                requests={gw.deposits}
                mayDecide={isAdmin}
                onSettle={(id, approve, detail) => gw.adminSettle(id, approve, detail)}
                onChanged={() => {
                  gw.adminDepositRequests()
                  gw.adminOverview()
                }}
              />
            ) : null}

            {tab === 'Audit' ? (
              <div className="table-wrap">
                <table className="table">
                  <thead>
                    <tr>
                      <th style={{ width: 70 }}>Seq</th>
                      <th style={{ width: 150 }}>When</th>
                      <th style={{ width: 150 }}>Who</th>
                      <th style={{ width: 120 }}>What</th>
                      <th style={{ width: 70 }}>Account</th>
                      <th>Detail</th>
                    </tr>
                  </thead>
                  <tbody>
                    {gw.audit.map((line) => (
                      <tr key={line.seq}>
                        <td className="mono dim">{line.seq}</td>
                        <td className="mono dim">{when(line.at)}</td>
                        <td>{line.by === 0 ? <span className="dim">platform</span> : (line.byLogin ?? `user ${line.by}`)}</td>
                        <td>{auditWords(line)}</td>
                        <td className="mono">{line.account}</td>
                        <td className="mono dim">{line.detail}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : null}

          </div>
          <div className="blotter-tabs blotter-tabs-foot">
            {TABS[isAdmin ? 'admin' : 'manager'].map((t) => (
              <button
                key={t}
                type="button"
                className={t === tab ? 'on' : undefined}
                onClick={() => setTab(t)}
              >
                {t}
                {tabCount(t) === null ? null : (
                  <span className="tab-count">({tabCount(t)})</span>
                )}
              </button>
            ))}
            <div className="grow" />
            {/* Said every time, until it is dealt with. This login can take
                money out of a client's account on a password alone, and a
                warning somebody has to go looking for is a warning nobody
                reads. */}
            {gw.session?.secondFactorWanted ? (
              <button
                type="button"
                className="mgr-nag"
                onClick={() => setSheet('second')}
                title="Set up an authenticator on this login"
              >
                This login has only a password — set up two-factor
              </button>
            ) : (
              <span className="mgr-hint dim">
                Right-click an account, a position or the background.
              </span>
            )}
          </div>

        </section>
        ) : null}
      </div>

      {menu ? (
        <ContextMenu
          at={{ x: menu.x, y: menu.y }}
          subject={menu.subject}
          items={menu.items}
          onClose={() => setMenu(null)}
        />
      ) : null}

      {sheet === 'payments' ? (
        <Sheet
          title="Payment methods"
          note="What clients are shown when they want to pay money in. Nothing here moves money: a client sees these details, pays, and says so — and the desk credits the account once it has seen the money arrive."
          onClose={() => setSheet(null)}
        >
          <PaymentMethods gw={gw} />
        </Sheet>
      ) : null}

      {sheet === 'second' ? (
        <Sheet
          title="Two-factor"
          note="A second thing somebody has to have, as well as the password. This login can move money out of a client's account, which is why it is offered here and not to clients."
          onClose={() => setSheet(null)}
        >
          <SecondFactor
            on={gw.session?.secondFactor ?? false}
            secret={gw.secondFactorSecret}
            onBegin={() => gw.beginSecondFactor()}
            onConfirm={(code) => gw.confirmSecondFactor(code)}
            onStop={(code) => gw.stopSecondFactor(code)}
          />
        </Sheet>
      ) : null}

      {sheet === 'pricing' ? (
        <Sheet
          title="Pricing"
          note="What the house earns, per instrument. Markup widens the spread and is applied before a client is quoted, so they deal at the price they saw. Commission leaves the balance on opening and again on closing."
          onClose={() => setSheet(null)}
        >
          <PricingSheet
            pricing={gw.pricing}
            mayEdit={isAdmin}
            onSetMarkup={(i, v) => gw.adminSetMarkup(i, v)}
            onSetCommission={(i, v) => gw.adminSetCommission(i, v)}
            onChanged={() => gw.markups()}
          />
        </Sheet>
      ) : null}

      {sheet === 'books' ? (
        <Sheet
          title="Trial balance"
          note="Every currency must read balanced. Money is only ever moved between books, never created, so anything else is our bug."
          onClose={() => setSheet(null)}
        >
          <div className="sheet-trial">
            {(gw.books?.trial ?? []).map((t) => (
              <div key={t.currency} className="mgr-data-row">
                <span className="mono">{t.currency}</span>
                <span className={`mono${t.residual === 0n ? ' profit' : ' loss'}`}>
                  {t.residual === 0n ? 'balanced' : showMoney(t.residual)}
                </span>
              </div>
            ))}
          </div>
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th style={{ width: 90 }}>Party</th>
                  <th>Book</th>
                  <th style={{ width: 60 }}>Ccy</th>
                  <th className="num" style={{ width: 130 }}>
                    Balance
                  </th>
                </tr>
              </thead>
              <tbody>
                {(gw.books?.lines ?? []).map((line) => (
                  <tr key={`${line.owner}-${line.book}-${line.currency}`}>
                    <td className={line.house ? 'mono' : 'mono dim'}>
                      {line.house ? 'house' : line.owner}
                    </td>
                    <td>{bookName(line.book)}</td>
                    <td className="dim">{line.currency}</td>
                    <td className={`num mono${line.amount < 0n ? ' loss' : ''}`}>
                      {showMoney(line.amount)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Sheet>
      ) : null}

      {ask ? (
        <Ask
          title={ask.title}
          {...(ask.subject === undefined ? {} : { subject: ask.subject })}
          {...(ask.icon === undefined ? {} : { icon: ask.icon })}
          {...(ask.note === undefined ? {} : { note: ask.note })}
          fields={ask.fields}
          submitLabel={ask.submitLabel}
          {...(ask.tone === undefined ? {} : { tone: ask.tone })}
          onCancel={() => setAsk(null)}
          onSubmit={(values) => {
            setAsk(null)
            ask.run(values)
          }}
        />
      ) : null}
    </div>
  )
}

/**
 * Where clients send money, as the desk sets it up.
 *
 * Switching a method off is how one is retired — not deleting it. A client
 * may already be paying to that wallet, and the desk has to be able to
 * recognise the address when the money lands.
 */
function PaymentMethods({ gw }: { gw: GatewayView }) {
  const [rail, setRail] = useState<Rail>('crypto')
  const [label, setLabel] = useState('')
  const [address, setAddress] = useState('')
  const [note, setNote] = useState('')
  const [editing, setEditing] = useState<number | null>(null)

  const clear = () => {
    setEditing(null)
    setLabel('')
    setAddress('')
    setNote('')
  }

  const save = (enabled: boolean) => {
    if (label.trim() === '' || address.trim() === '') return
    gw.adminPutPaymentMethod({
      id: editing,
      rail,
      label: label.trim(),
      address: address.trim(),
      note: note.trim(),
      enabled,
    })
    clear()
    window.setTimeout(() => gw.paymentMethods(), 250)
  }

  return (
    <>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th style={{ width: 62 }}>Rail</th>
              <th style={{ width: 130 }}>Name</th>
              <th>Pay to</th>
              <th style={{ width: 70 }}>Shown</th>
              <th style={{ width: 120 }} />
            </tr>
          </thead>
          <tbody>
            {gw.methods.map((m) => (
              <tr key={m.id}>
                <td className="dim">{RAIL_WORD[m.rail]}</td>
                <td>{m.label}</td>
                <td className="mono" style={{ overflowWrap: 'anywhere' }}>
                  {m.address}
                  {m.note ? <div className="dim">{m.note}</div> : null}
                </td>
                <td>
                  {m.enabled ? <Pill tone="ok">yes</Pill> : <span className="dim">no</span>}
                </td>
                <td>
                  <div className="row" style={{ gap: 4 }}>
                    <button
                      type="button"
                      className="mgr-mini"
                      onClick={() => {
                        setEditing(m.id)
                        setRail(m.rail)
                        setLabel(m.label)
                        setAddress(m.address)
                        setNote(m.note)
                      }}
                    >
                      Edit
                    </button>
                    <button
                      type="button"
                      className="mgr-mini"
                      onClick={() => {
                        gw.adminPutPaymentMethod({
                          id: m.id,
                          rail: m.rail,
                          label: m.label,
                          address: m.address,
                          note: m.note,
                          enabled: !m.enabled,
                        })
                        window.setTimeout(() => gw.paymentMethods(), 250)
                      }}
                    >
                      {m.enabled ? 'Hide' : 'Show'}
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {gw.methods.length === 0 ? (
          <p className="table-empty dim">
            No way to pay in yet. Clients see nothing until you add one.
          </p>
        ) : null}
      </div>

      <div className="mgr-add" style={{ paddingTop: 10 }}>
        <span className="cap">
          {editing === null ? 'Add a way to pay' : `Editing method ${editing}`}
        </span>
        <select
          className="mgr-input"
          value={rail}
          aria-label="Rail"
          onChange={(e) => setRail(e.target.value as Rail)}
        >
          <option value="crypto">Crypto</option>
          <option value="upi">UPI</option>
          <option value="bank">Bank transfer</option>
        </select>
        <input
          className="mgr-input"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder={rail === 'crypto' ? 'USDT (TRC-20)' : rail === 'upi' ? 'UPI' : 'HDFC Bank'}
          aria-label="Name"
        />
        <input
          className="mgr-input mono"
          value={address}
          onChange={(e) => setAddress(e.target.value)}
          placeholder={
            rail === 'crypto'
              ? 'wallet address'
              : rail === 'upi'
                ? 'name@bank'
                : 'account number, IFSC, holder'
          }
          spellCheck={false}
          aria-label="Pay to"
        />
        <input
          className="mgr-input"
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="anything they must do to be credited (optional)"
          aria-label="Note"
        />
        <div className="row" style={{ gap: 4 }}>
          <button
            type="button"
            className="mgr-go"
            disabled={label.trim() === '' || address.trim() === ''}
            onClick={() => save(true)}
          >
            {editing === null ? 'Add and show it' : 'Save and show it'}
          </button>
          <button
            type="button"
            className="mgr-mini"
            disabled={label.trim() === '' || address.trim() === ''}
            onClick={() => save(false)}
          >
            Save hidden
          </button>
          {editing === null ? null : (
            <button type="button" className="mgr-mini" onClick={clear}>
              Cancel
            </button>
          )}
        </div>
        <p className="mgr-note">
          Check the address character by character before you show it. A wrong
          wallet sends a client&rsquo;s money somewhere nobody can reach, and
          nothing here can undo that.
        </p>
      </div>
    </>
  )
}

/**
 * A branch of the navigator tree.
 *
 * The twisty is the whole control: the label opens and closes with it,
 * because a folder you can only open by hitting a nine-pixel triangle is a
 * folder nobody opens.
 */
function Branch({
  label,
  depth,
  kind,
  count,
  shut,
  onFold,
}: {
  label: string
  depth: number
  kind: 'root' | 'folder' | 'manager' | 'star' | 'people' | 'loose'
  count?: number
  shut: boolean
  onFold(): void
}) {
  return (
    <button
      type="button"
      className={`mgr-branch mgr-branch-${kind}`}
      style={{ paddingLeft: 6 + depth * 13 }}
      onClick={onFold}
      aria-expanded={!shut}
    >
      <span className="mgr-twisty" aria-hidden="true">
        {shut ? '\u25b8' : '\u25be'}
      </span>
      <span className="mgr-branch-icon" aria-hidden="true">
        {BRANCH_GLYPH[kind]}
      </span>
      <span className="mgr-branch-name">{label}</span>
      {count === undefined ? null : <span className="mgr-branch-count">{count}</span>}
    </button>
  )
}

const BRANCH_GLYPH: Record<string, string> = {
  root: '\u25c6',
  folder: '\u25a4',
  manager: '\u25a3',
  star: '\u2605',
  people: '\u25cf',
  loose: '\u25a1',
}

/** One account in the tree. */
function AccountRow({
  row,
  depth,
  selected,
  showManager,
  favourite,
  onPick,
  onStar,
  onMenu,
}: {
  row: { account: AdminAccount; kyc: Kyc; manager: string | null }
  depth: number
  selected: number
  /** The book it sits in, for the one person who has more than one. */
  showManager: boolean
  favourite: boolean
  onPick(): void
  onStar(): void
  onMenu(x: number, y: number): void
}) {
  const a = row.account
  return (
    <button
      type="button"
      className={`mgr-acct${a.id === selected ? ' on' : ''}`}
      style={{ paddingLeft: 6 + depth * 13 }}
      onClick={onPick}
      onContextMenu={(e) => {
        e.preventDefault()
        e.stopPropagation()
        onMenu(e.clientX, e.clientY)
      }}
      title={`${a.ownerLogin ?? 'unclaimed'} \u2014 ${a.live ? 'live' : daysLeft(a)}${
        row.manager ? ` \u2014 managed by ${row.manager}` : ''
      }`}
    >
      <span className="mgr-acct-line">
        <span className={`mgr-dot ${stateOf(a)}`} aria-hidden="true" />
        <span className="mono mgr-acct-id">{a.id}</span>
        <span className="mgr-acct-who">{a.ownerLogin ?? 'unclaimed'}</span>
        <span
          className={`mgr-star${favourite ? ' on' : ''}`}
          role="button"
          tabIndex={-1}
          aria-label={favourite ? 'Unpin this account' : 'Pin this account'}
          title={favourite ? 'Unpin this account' : 'Pin this account'}
          onClick={(e) => {
            e.stopPropagation()
            onStar()
          }}
        >
          {favourite ? '\u2605' : '\u2606'}
        </span>
        <span className={`mgr-kyc kyc-${row.kyc}`}>{kycWords(row.kyc)}</span>
      </span>
      {showManager && row.manager ? (
        <span className="mgr-acct-book">under {row.manager}</span>
      ) : null}
    </button>
  )
}

/**
 * The tick tape: every price this symbol has printed since the tab opened.
 *
 * Kept in the component rather than the feed, because it is a view of what
 * has just happened and nothing else needs it. Newest first, and bounded —
 * a tape that grows forever is a leak with a nice name.
 */
function Ticks({ instrument, quote }: { instrument: Instrument; quote: FeedQuote | undefined }) {
  const [tape, setTape] = useState<readonly { at: number; bid: Price; ask: Price }[]>([])

  // A new symbol is a new tape: the old prices were a different market.
  useEffect(() => setTape([]), [instrument.id])

  useEffect(() => {
    if (!quote) return
    setTape((prev) => {
      const head = prev[0]
      if (head && head.bid === quote.bid && head.ask === quote.ask) return prev
      return [{ at: Date.now(), bid: quote.bid, ask: quote.ask }, ...prev].slice(0, TAPE_KEPT)
    })
  }, [quote, instrument.id])

  if (tape.length === 0) {
    return <p className="mgr-empty dim">Waiting for {instrument.symbol} to print.</p>
  }

  return (
    <div className="table-wrap">
      <table className="table mw">
        <thead>
          <tr>
            <th style={{ width: 74 }}>Time</th>
            <th className="num">Bid</th>
            <th className="num">Ask</th>
          </tr>
        </thead>
        <tbody>
          {tape.map((t, i) => {
            const before = tape[i + 1]
            return (
              <tr key={`${t.at}-${i}`}>
                <td className="mono dim">{CLOCK.format(new Date(t.at))}</td>
                <td className={`num mono${before ? (t.bid > before.bid ? ' profit' : t.bid < before.bid ? ' loss' : '') : ''}`}>
                  {showPrice(t.bid, instrument.displayDigits)}
                </td>
                <td className={`num mono${before ? (t.ask > before.ask ? ' profit' : t.ask < before.ask ? ' loss' : '') : ''}`}>
                  {showPrice(t.ask, instrument.displayDigits)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

/** How much tape is kept. Enough to see a move, not enough to matter. */
const TAPE_KEPT = 200

const CLOCK = new Intl.DateTimeFormat(undefined, {
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
  hour12: false,
})

function Figure({
  label,
  value,
  tone,
  strip = false,
}: {
  label: string
  value: string
  tone?: string | undefined
  /** One line rather than two, for the status strip under the chart. */
  strip?: boolean
}) {
  return (
    <div className={strip ? 'mgr-stat' : 'mgr-figure'}>
      <span className="cap">{label}</span>
      <span className="mono" style={tone ? { color: tone } : undefined}>
        {value}
      </span>
    </div>
  )
}

/**
 * The dot beside an account in the navigator.
 *
 * Suspended first, because it is the one that stops a trade; then a demo
 * running out, which stops one later; then ordinary.
 */
function stateOf(a: AdminAccount): string {
  if (!a.tradeable) return 'dot-bad'
  if (expiredSoon(a)) return 'dot-warn'
  return 'dot-ok'
}

/**
 * Why the deal ticket is inert, when it is.
 *
 * The gateway refuses all of these itself — this is so the operator reads the
 * reason before sending an order rather than after.
 */
function dealRefusal(account: AdminAccount | null, connected: boolean): string | null {
  if (!connected) return 'Gateway offline'
  if (!account) return null
  if (!account.tradeable) return `Account ${account.id} is suspended`
  if (!account.live && account.demoExpiresAt !== null && account.demoExpiresAt <= Date.now())
    return `Demo ${account.id} has expired`
  return null
}

function describeAction(kind: AdminActionKind, id: number, amount: string): string {
  switch (kind) {
    case 'deposit':
      return `Deposit ${amount} to account ${id}?`
    case 'withdraw':
      return `Withdraw ${amount} from account ${id}?`
    case 'credit':
      return `Set the bonus on account ${id} to ${amount}?`
    case 'suspend':
      return `Suspend account ${id}? It will not be able to open positions.`
    case 'reinstate':
      return `Let account ${id} trade again?`
    case 'hedging':
      return `Let account ${id} hold both sides of an instrument?`
    case 'netting':
      return `Net opposing orders off on account ${id} again?`
    case 'live':
      return `Make account ${id} LIVE? The money in it becomes the client's.`
    case 'demo':
      return `Put account ${id} back to demo?`
    default:
      return `Give account ${id} another thirty days?`
  }
}

/** What a statement line is, in the words a client would use. */
function statementWords(kind: string): string {
  switch (kind) {
    case 'deposit':
      return 'Deposit'
    case 'withdrawal':
      return 'Withdrawal'
    case 'bonus':
      return 'Bonus'
    case 'trade':
      return 'Trade closed'
    case 'financing':
      return 'Overnight financing'
    case 'stopOut':
      return 'Closed to protect the account'
    case 'stopLoss':
      return 'Stop loss'
    case 'takeProfit':
      return 'Take profit'
    case 'adjustment':
      return 'Adjustment by the desk'
    case 'transferOut':
      return 'Moved to another account'
    case 'transferIn':
      return 'Moved from another account'
    default:
      return kind
  }
}

/**
 * How an account introduces itself at the top of its own menu.
 *
 * The number first, because that is what the desk says out loud; the client
 * under it; and live or demo as a pill, because which of the two it is
 * decides whether the money in it is somebody's.
 */
function subjectOf(a: AdminAccount): MenuSubject {
  const who = a.ownerLogin ?? 'unclaimed'
  return {
    title: String(a.id),
    subtitle: who,
    badge: a.live ? 'LIVE' : 'DEMO',
    badgeTone: a.live ? 'ok' : 'warn',
    initial: who.slice(0, 1).toUpperCase(),
  }
}

/** Tints a margin level an operator should look at. */
function levelTone(bp: number | null): string {
  if (bp === null) return ' dim'
  if (bp < 10_000) return ' loss'
  if (bp < 20_000) return ' warn'
  return ''
}

const DAY_MS = 24 * 60 * 60 * 1000

/** How long a demo account has left, in words. */
function daysLeft(a: AdminAccount): string {
  if (a.live || a.demoExpiresAt === null) return 'Live account'
  const left = a.demoExpiresAt - Date.now()
  if (left <= 0) return 'expired'
  const days = Math.ceil(left / DAY_MS)
  return days === 1 ? '1 day left' : `${days} days left`
}

/** True when a demo account is inside its last week, or already out of time. */
const expiredSoon = (a: AdminAccount): boolean =>
  !a.live && a.demoExpiresAt !== null && a.demoExpiresAt - Date.now() < 7 * DAY_MS

const kycWords = (kyc: Kyc): string =>
  kyc === 'approved'
    ? 'verified'
    : kyc === 'submitted'
      ? 'waiting'
      : kyc === 'rejected'
        ? 'refused'
        : 'unverified'

const WHEN = new Intl.DateTimeFormat('en-GB', {
  day: '2-digit',
  month: 'short',
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
})

const when = (ms: number) => (ms > 0 ? WHEN.format(new Date(ms)) : '—')

const symbolOf = (list: readonly Instrument[], id: number) =>
  list.find((i) => i.id === id)?.symbol ?? `#${id}`

const digitsOf = (list: readonly Instrument[], id: number) =>
  list.find((i) => i.id === id)?.displayDigits ?? 2

const contractOf = (list: readonly Instrument[], id: number) =>
  list.find((i) => i.id === id)?.contractSize ?? qty('1')

/** Entry lines for whatever the selected account holds on this symbol. */
function positionLines(positions: readonly GatewayPosition[], instrument: Instrument) {
  return positions
    .filter((p) => p.instrumentId === instrument.id)
    .map((p) => ({
      price: p.openPrice,
      label: `${p.side.toUpperCase()} ${showQtyAsLots(p.qty, instrument.contractSize, 3)} @ ${showPrice(p.openPrice, instrument.displayDigits)}`,
      tone: p.side,
    }))
}

/**
 * Orders already resting on this symbol.
 *
 * Labelled the way the ticket labels them — `Buy BL 4014.10 (3586475)` —
 * because the line on the chart and the button that made it should use the
 * same words, and the ticket number is how it is cancelled.
 */
function restingLines(orders: readonly GatewayPending[], instrument: Instrument) {
  return orders
    .filter((o) => o.instrumentId === instrument.id)
    .map((o) => ({
      price: o.triggerPrice,
      label: `${o.side === 'buy' ? 'Buy' : 'Sell'} ${shortKind(o.side, o.kind)} ${showPrice(o.triggerPrice, instrument.displayDigits)} (${o.ticket})`,
      tone: o.side,
      dashed: true,
    }))
}

/** Where the ticket is pointed but has not yet sent. */
function aimLines(aiming: readonly Resting[], instrument: Instrument) {
  return aiming.map((a) => ({
    price: a.price,
    label: `${a.side === 'buy' ? 'Buy' : 'Sell'} ${shortKind(a.side, a.kind)} ${showPrice(a.price, instrument.displayDigits)}`,
    tone: 'accent' as const,
    dashed: true,
  }))
}

/** `BL`, `BS`, `SL`, `SS` — the ticket's own shorthand. */
function shortKind(side: string, kind: string): string {
  return (side === 'buy' ? 'B' : 'S') + (kind === 'limit' ? 'L' : kind === 'stop' ? 'S' : '')
}

function bookName(book: string): string {
  switch (book) {
    case 'ClientCash':
      return 'Client cash'
    case 'Bonus':
      return 'Bonus'
    case 'RealisedPnl':
      return 'Realised P&L'
    case 'Swap':
      return 'Overnight financing'
    case 'PspClearing':
      return 'Payments clearing'
    case 'Commission':
      return 'Commission'
    case 'Adjustment':
      return 'Adjustments'
    case 'LpMargin':
      return 'LP margin'
    case 'FxConversion':
      return 'FX conversion'
    default:
      return book
  }
}

function auditWords(line: AuditLine): string {
  switch (line.action) {
    case 'deposit':
      return 'Deposit'
    case 'withdrawal':
      return 'Withdrawal'
    case 'creditSet':
      return 'Bonus set'
    case 'suspended':
      return 'Suspended'
    case 'reinstated':
      return 'Reinstated'
    case 'order':
      return 'Order'
    case 'restingOrder':
      return 'Resting order'
    case 'accountOpened':
      return 'Account opened'
    case 'ownerSet':
      return 'Owner set'
    case 'adjustment':
      return 'Adjustment'
    case 'transfer':
      return 'Transfer'
    case 'priceCorrected':
      return 'Price corrected'
    default:
      return line.action
  }
}

