/**
 * The client terminal.
 *
 * Two sources, in priority order:
 *
 * 1. **The gateway.** When it is running it is the authority — it lists the
 *    instruments, owns the prices, and computes every account figure. Orders
 *    go to it and come back as fills or rejections from the real engine.
 * 2. **The exchange feed.** When the gateway is down the terminal still shows
 *    live prices, and says plainly that trading is unavailable. It does NOT
 *    invent an account: a balance with nothing behind it is worse than none.
 *
 * Layout follows the MT4-lineage convention traders already know.
 */

import { Fragment, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { ChartPane } from '../components/ChartPane'
import { LevelCell } from '../components/LevelCell'
import { MarketWatch, SOURCE_NAME } from '../components/MarketWatch'
import type { PriceLine } from '../components/PriceChart'
import { Sheet } from '../components/Sheet'
import { Toasts } from '../components/Toasts'
import { halfOf, stepOf } from '../lib/positions'
import { Wallet } from '../components/Wallet'
import type { WalletTab } from '../components/Wallet'
import { Button, DataTable, Panel, Pill, Readout } from '../components/primitives'
import type { Column } from '../components/primitives'
import type {
  AmendReason,
  GatewayEvent,
  GatewayPending,
  GatewayPosition,
  StatementLine,
} from '../feed/gateway'
import type { FeedQuote, FeedState, Interval } from '../feed/types'
import { useFeed } from '../feed/useFeed'
import type { GatewayView } from '../feed/useGateway'
import { useChimes } from '../feed/useChimes'
import { wake } from '../lib/chime'
import type { Instrument, Notice, OrderType, RejectReason } from '../lib/domain'
import { ORDER_TYPE_LABEL, rejectionCopy } from '../lib/domain'
import { DEFAULT_INSTRUMENT, INSTRUMENTS } from '../lib/instruments'
import type { Money, Price, Qty } from '../lib/money'
import {
  convertPer,
  marginRequired,
  notional,
  price as parsePrice,
  qty,
  showMarginLevel,
  showMoney,
  showPrice,
  showQty,
  showQtyAsLots,
  showSigned,
  sum,
} from '../lib/money'
import './ClientTerminal.css'


const BLOTTER_TABS = ['Trade', 'Net trade', 'Orders', 'History', 'Journal', 'Mailbox', 'News', 'Alerts'] as const

export interface ClientTerminalProps {
  /** The connection, opened once by the shell and shared. */
  readonly gw: GatewayView
}

export function ClientTerminal({ gw }: ClientTerminalProps) {

  const [interval, setIntervalChoice] = useState<Interval>('5m')
  const [tab, setTab] = useState<(typeof BLOTTER_TABS)[number]>('Trade')
  const [selected, setSelected] = useState(DEFAULT_INSTRUMENT.id)
  const [volume, setVolume] = useState<Qty>(qty('0.10'))
  const [orderKind, setOrderKind] = useState<OrderType>('market')
  const [priceText, setPriceText] = useState('')
  const [ticketOpen, setTicketOpen] = useState(true)
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(new Set())
  /**
   * Which side of the wallet is open, or null for closed.
   *
   * One sheet rather than two, because paying in and withdrawing are the
   * same errand from the client's side — "my money" — and they are reached
   * from two buttons that land on the right tab.
   */
  const [wallet, setWallet] = useState<WalletTab | null>(null)

  // Instruments come from the gateway when it is up; the static list is only a
  // fallback so the market watch is not empty while it is down.
  const instruments: readonly Instrument[] = gw.connected ? gw.instruments : INSTRUMENTS
  const exchange = useFeed(INSTRUMENTS, { enabled: !gw.connected })
  const quotes: ReadonlyMap<number, FeedQuote> = gw.connected ? gw.quotes : exchange.quotes

  const instrument =
    instruments.find((i) => i.id === selected) ?? instruments[0] ?? DEFAULT_INSTRUMENT
  const quote = quotes.get(instrument.id)

  // What the engine says about trading it. Offline, nothing is tradeable.
  const listing = gw.connected ? gw.instruments.find((i) => i.id === instrument.id) : undefined
  const viewOnly = listing?.tradeable === false



  const step = gw.connected
    ? (gw.instruments.find((i) => i.id === instrument.id)?.qtyStep ?? qty('0.01'))
    : qty('0.01')

  // The statement is asked for when its tab is opened, and after anything
  // that could have moved money. A tab nobody is looking at is not polled.
  useEffect(() => {
    if (tab !== 'History' || !gw.connected) return
    gw.requestStatement()
  }, [tab, gw.connected, gw.requestStatement, gw.events.length, gw.session?.account])

  // Keep the size legal when the trader changes symbol.
  useEffect(() => {
    const listed = gw.instruments.find((i) => i.id === instrument.id)
    if (!listed) return
    setVolume((v) => (v < listed.minQty ? listed.minQty : v))
  }, [instrument.id, gw.instruments])

  const session = gw.session
  const positions = gw.positions

  // Fills, stop-outs, margin calls and protections.
  useChimes(gw.events, positions)
  // Swap counts toward floating, as it does toward the engine's equity figure.
  const floating = useMemo(
    () => sum(positions.map((p) => (p.profit + p.swap) as Money)),
    [positions],
  )
  useEffect(() => {
    if (wallet === null) return
    gw.paymentMethods()
    gw.payoutMethods()
  }, [wallet, gw])

  const account = gw.account

  // Notional and margin arise in the instrument's quote currency, and the
  // ticket shows the account's. A rate of zero means the gateway has no rate
  // for it, and then there is no figure to show — not a figure to guess.
  const inAccountCurrency = (amount: Money): Money | null => {
    if (quote?.rate === undefined) return amount
    return quote.rate === 0n ? null : convertPer(amount, quote.rate)
  }
  const ticketNotional = quote ? inAccountCurrency(notional(quote.ask, volume)) : null
  const ticketMargin =
    quote && account
      ? inAccountCurrency(marginRequired(quote.ask, volume, account.leverage))
      : null

  const notices = useMemo(
    () => gw.events.map((e, i) => toNotice(e, i, instruments)).filter((n) => !dismissed.has(n.id)),
    [gw.events, dismissed, instruments],
  )
  const dismiss = (id: string) => setDismissed((s) => new Set(s).add(id))

  const adjust = (by: 1n | -1n) =>
    setVolume((v) => {
      const next = (v + step * by) as Qty
      return next > 0n ? next : v
    })

  // A resting order needs a price, and an empty box is a worse default than a
  // sensible one. Seed it a little away from the market in the direction the
  // order type implies, so the trader adjusts rather than types from nothing.
  const chooseKind = (kind: OrderType) => {
    setOrderKind(kind)
    if (kind !== 'market' && quote && priceText === '') {
      const away = quote.ask / 100n
      const seed = kind === 'limit' ? quote.ask - away : quote.ask + away
      setPriceText(showPrice(seed as Price, instrument.displayDigits))
    }
  }

  /** The typed price, or null when it is empty or not a number. */
  const restingPrice = (): Price | null => {
    if (orderKind === 'market') return null
    try {
      const p = parsePrice(priceText.trim())
      return p > 0n ? p : null
    } catch {
      return null
    }
  }

  const submit = (side: 'buy' | 'sell') => {
    if (orderKind === 'market') {
      gw.placeOrder({ instrumentId: instrument.id, side, kind: 'market', qty: volume })
      return
    }
    const p = restingPrice()
    if (!p) return
    gw.placeOrder({ instrumentId: instrument.id, side, kind: orderKind, qty: volume, price: p })
  }

  const canSubmit =
    gw.connected && !!quote && !viewOnly && (orderKind === 'market' || restingPrice() !== null)
  const contract = instrument.contractSize

  /**
   * The account statement: every movement, and what it left behind.
   *
   * The balance column is the point of it. A trader asking "why is my balance
   * this" is asking to follow it down the page, so the figure on each line is
   * the balance *after* that movement — taken from the engine as it applied
   * it, not re-added here.
   */
  const statementColumns: readonly Column<StatementLine>[] = [
    {
      key: 'when',
      header: 'When',
      width: 150,
      render: (l) => <span className="mono dim">{stamp(l.at)}</span>,
    },
    { key: 'kind', header: 'Type', width: 110, render: (l) => movementWords(l.kind) },
    {
      key: 'ticket',
      header: 'Ticket',
      width: 72,
      render: (l) =>
        l.ticket === null ? <span className="dim">—</span> : <span className="mono dim">{l.ticket}</span>,
    },
    {
      key: 'amount',
      header: 'Amount',
      align: 'right',
      render: (l) => (
        <span className={l.amount < 0n ? 'loss' : 'profit'}>{showSigned(l.amount)}</span>
      ),
    },
    {
      key: 'balance',
      header: 'Balance',
      align: 'right',
      render: (l) => <span className="mono">{showMoney(l.balance)}</span>,
    },
  ]

  const pendingColumns: readonly Column<GatewayPending>[] = [
    { key: 'ticket', header: 'Ticket', width: 68, render: (o) => <span className="mono dim">{o.ticket}</span> },
    { key: 'side', header: 'Side', render: (o) => <Pill tone={o.side}>{o.side}</Pill> },
    {
      key: 'kind',
      header: 'Type',
      render: (o) => <span className="dim">{ORDER_TYPE_LABEL[o.kind]}</span>,
    },
    {
      key: 'sym',
      header: 'Symbol',
      render: (o) => <span className="sym">{symbolOf(instruments, o.instrumentId)}</span>,
    },
    { key: 'vol', header: 'Volume', align: 'right', render: (o) => showQtyAsLots(o.qty, contractOf(instruments, o.instrumentId), 3) },
    {
      key: 'trigger',
      header: 'Trigger',
      align: 'right',
      render: (o) => showPrice(o.triggerPrice, digitsOf(instruments, o.instrumentId)),
    },
    {
      key: 'distance',
      header: 'Distance',
      align: 'right',
      render: (o) => {
        const q = quotes.get(o.instrumentId)
        if (!q) return <span className="dim">—</span>
        const ref = o.side === 'buy' ? q.ask : q.bid
        const away = (o.triggerPrice - ref) as Price
        return (
          <span className="dim">
            {showPrice(away, digitsOf(instruments, o.instrumentId))}
          </span>
        )
      },
    },
    {
      key: 'cancel',
      header: '',
      width: 62,
      render: (o) => (
        <Button tone="danger" onClick={() => gw.cancelOrder(o.ticket)}>
          Cancel
        </Button>
      ),
    },
  ]

  const columns: readonly Column<GatewayPosition>[] = [
    { key: 'ticket', header: 'Ticket', width: 68, render: (p) => <span className="mono dim">{p.ticket}</span> },
    { key: 'side', header: 'Type', render: (p) => <Pill tone={p.side}>{p.side}</Pill> },
    {
      key: 'sym',
      header: 'Symbol',
      render: (p) => <span className="sym">{symbolOf(instruments, p.instrumentId)}</span>,
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
      render: (p) => showPrice(p.openPrice, digitsOf(instruments, p.instrumentId)),
    },
    {
      key: 'now',
      header: 'Current',
      align: 'right',
      render: (p) =>
        p.current ? showPrice(p.current, digitsOf(instruments, p.instrumentId)) : <span className="dim">—</span>,
    },
    {
      key: 'swap',
      header: 'Swap',
      align: 'right',
      render: (p) =>
        p.swap === 0n ? (
          <span className="dim">0.00</span>
        ) : (
          <span className={p.swap < 0n ? 'loss' : 'profit'}>{showSigned(p.swap)}</span>
        ),
    },
    {
      key: 'sl',
      header: 'S/L',
      align: 'right',
      width: 92,
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
      width: 92,
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
      key: 'pnl',
      header: 'P&L',
      align: 'right',
      render: (p) => <span className={p.profit < 0n ? 'loss' : 'profit'}>{showSigned(p.profit)}</span>,
    },
    {
      key: 'close',
      header: '',
      width: 108,
      render: (p) => (
        <span className="row" style={{ gap: 4 }}>
          {/*
            Half, rather than a dialog asking how much. Taking half off is
            what a trader does to a position that has run, and offering the
            common case as one press beats offering every case as three.
            The exact figure is still available by right-clicking.
          */}
          <Button
            disabled={halfOf(p.qty, stepOf(gw.instruments, p.instrumentId)) === null}
            onClick={() => {
              const half = halfOf(p.qty, stepOf(gw.instruments, p.instrumentId))
              // Never a fallback to closing the lot: a Half that sometimes
              // closes everything is worse than one that is greyed out,
              // because the surprise costs a position.
              if (half === null) return
              gw.closePosition(p.ticket, showQty(half))
            }}
          >
            Half
          </Button>
          <Button tone="danger" onClick={() => gw.closePosition(p.ticket)}>
            Close
          </Button>
        </span>
      ),
    },
  ]

  return (
    <div className="terminal" onPointerDownCapture={wake}>
      {/* ── Command bar ── */}
      <div className="cmdbar">
        <div className="brand">
          <svg width="17" height="17" viewBox="0 0 20 20" fill="none" stroke="var(--accent)" strokeWidth={1.7} strokeLinecap="round" strokeLinejoin="round">
            <path d="M3 15.5 L7 9.5 L11 12 L17 4" />
            <path d="M3 17.5 L17 17.5" />
          </svg>
          <span className="brand-name">FERARO</span>
        </div>
        <nav className="menu">
          {['Home', 'View', 'Charts', 'Analysis', 'Scripts', 'Help'].map((m, i) => (
            <span key={m} className={i === 0 ? 'menu-on' : undefined}>
              {m}
            </span>
          ))}
        </nav>
        <div className="grow" />
        <div className="row" style={{ gap: 7 }}>
          <span className={`dot dot-${dotFor(gw.status)}`} />
          {/* An administrator acts on one account at a time, and which one it
              is must never be a guess. A client with one account sees it as
              plain text rather than a control that does nothing. */}
          {session && session.accounts.length > 1 ? (
            <select
              className="acct-pick mono"
              value={session.account}
              onChange={(e) => gw.selectAccount(Number(e.target.value))}
              aria-label="Account"
            >
              {session.accounts.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.id} · {a.currency} · {showMoney(a.equity)}
                  {a.id === session.account ? '' : ''}
                </option>
              ))}
            </select>
          ) : (
            <span className="mono" style={{ fontSize: 11 }}>
              {account
                ? `${account.id} · ${account.currency} · 1:${account.leverage}`
                : 'not signed in'}
            </span>
          )}
          {/* Demo or live, in the title bar: a trader should never be unsure
              whether the money they are risking is real. */}
          <span className={`cap${account?.live ? ' acct-live-tag' : ''}`}>
            {!gw.connected
              ? 'Offline'
              : account?.live
                ? 'Live'
                : account?.demoExpiresAt
                  ? `Demo · ${demoDaysLeft(account.demoExpiresAt)}`
                  : 'Demo'}
          </span>
          {session ? (
            <span className="cap" title={`Signed in as ${session.login}`}>
              {session.login}
              {session.role === 'admin' ? ' · admin' : ''}
            </span>
          ) : null}
          <button type="button" className="cap sign-out" onClick={gw.signOut}>
            Sign out
          </button>
        </div>
      </div>

      {/* ── Toolbar ── */}
      <div className="toolbar">
        <ToolGroup name="Trade">
          <ToolButton label={'New\nposition'} tone="var(--buy)" onClick={() => setTicketOpen(true)} />
          <ToolButton label={'Entry\norder'} />
          <ToolButton label={'Close\nall'} tone="var(--sell)" onClick={() => positions.forEach((p) => gw.closePosition(p.ticket))} />
          <ToolButton label="Statement" />
        </ToolGroup>
        <ToolGroup name="Funds">
          <ToolButton
            label="Deposit"
            tone="var(--profit)"
            onClick={() => setWallet('in')}
          />
          <ToolButton label="Withdraw" onClick={() => setWallet('out')} />
        </ToolGroup>
        <ToolGroup name="Workspace">
          <ToolButton label="Indicators" />
          <ToolButton label="Alerts" tone="var(--warn)" />
          <ToolButton label="Options" />
        </ToolGroup>
        <div className="grow" />
        {account ? (
          <div className="account-strip">
            <Readout label="Balance" value={showMoney(account.balance)} />
            <Readout label="Credit" value={showMoney(account.credit)} tone="credit" />
            <Readout label="Equity" value={showMoney(account.equity)} />
            <Readout label="Margin" value={showMoney(account.usedMargin)} />
            <Readout label="Free margin" value={showMoney(account.freeMargin)} />
            <Readout
              label="Margin level"
              value={showMarginLevel(account.marginLevelBp)}
              tone={
                account.marginLevelBp !== null && account.marginLevelBp < 10_000 ? 'warn' : 'profit'
              }
            />
          </div>
        ) : (
          <div className="account-strip">
            <span className="cap">Gateway offline — prices only, no trading</span>
          </div>
        )}
      </div>

      {/* ── Body ── */}
      <div className="body">
        <div className="col-main">
          {/* The pane carries its own timeframe row and the bar it is on, so
              the panel heading is just the symbol. */}
          <Panel grow title={instrument.symbol}>
            <ChartPane
              gw={gw}
              instrument={instrument}
              interval={interval}
              onInterval={setIntervalChoice}
              last={quote?.bid}
              lines={positionLines(positions, instrument)}
            />
          </Panel>

          <section className="panel blotter">
            <div className="blotter-tabs">
              {BLOTTER_TABS.map((t) => (
                <button key={t} type="button" className={`tab${t === tab ? ' tab-on' : ''}`} onClick={() => setTab(t)}>
                  {t}
                  {t === 'Trade' && positions.length > 0 ? (
                    <span className="mono dim"> {positions.length}</span>
                  ) : null}
                  {t === 'Orders' && gw.pending.length > 0 ? (
                    <span className="mono dim"> {gw.pending.length}</span>
                  ) : null}
                </button>
              ))}
              <div className="grow" />
              <div className="row" style={{ gap: 12, padding: '0 12px' }}>
                <span className="cap">Floating</span>
                <span className={`mono ${floating < 0n ? 'loss' : 'profit'}`} style={{ fontSize: 13, fontWeight: 600 }}>
                  {showSigned(floating)}
                </span>
              </div>
            </div>
            {tab === 'Trade' ? (
              <DataTable
                columns={columns}
                rows={positions}
                rowKey={(p) => p.ticket}
                empty={gw.connected ? 'No open positions. Place one from the ticket.' : 'Gateway offline.'}
              />
            ) : tab === 'Orders' ? (
              <DataTable
                columns={pendingColumns}
                rows={gw.pending}
                rowKey={(o) => o.ticket}
                empty={
                  gw.connected
                    ? 'No resting orders. Choose Limit or Stop in the ticket.'
                    : 'Gateway offline.'
                }
              />
            ) : tab === 'History' ? (
              <DataTable
                columns={statementColumns}
                rows={gw.statement}
                rowKey={(l) => l.seq}
                empty={
                  gw.connected
                    ? 'Nothing has moved on this account yet.'
                    : 'Gateway offline.'
                }
              />
            ) : (
              <p className="table-empty dim">No {tab.toLowerCase()} entries.</p>
            )}
          </section>
        </div>

        <div className="col-dock">
          <Panel grow title="Market Watch" meta={`${instruments.length} symbols`}>
            <MarketWatch
              instruments={instruments}
              quotes={quotes}
              selected={instrument.id}
              onSelect={setSelected}
            />
          </Panel>

          <Panel title="My accounts" style={{ flex: 'none', maxHeight: 250 }}>
            <ul className="nav-tree">
              {(['Demo', 'Live'] as const).map((group) => {
                const mine = (session?.accounts ?? []).filter((a) =>
                  group === 'Live' ? a.live : !a.live,
                )
                if (mine.length === 0) return null
                return (
                  <Fragment key={group}>
                    <li className="nav-root">{group}</li>
                    {mine.map((a) => (
                      <li
                        key={a.id}
                        className={`nav-item mono${a.id === session?.account ? ' nav-on' : ''}`}
                        onClick={() => gw.selectAccount(a.id)}
                        title={
                          a.live
                            ? 'Live account'
                            : a.demoExpiresAt === null
                              ? 'Demo account'
                              : `Demo · ${demoDaysLeft(a.demoExpiresAt)}`
                        }
                      >
                        {a.id} · {showMoney(a.equity)}
                        {!a.live && a.demoExpiresAt !== null ? (
                          <span className="acct-left">{demoDaysLeft(a.demoExpiresAt)}</span>
                        ) : null}
                      </li>
                    ))}
                  </Fragment>
                )
              })}
            </ul>

            <div className="acct-actions">
              <button
                type="button"
                className="acct-open"
                disabled={!gw.connected}
                onClick={() => {
                  if (window.confirm('Open another demo account?')) gw.openAccount(false)
                }}
              >
                New demo account
              </button>
              {gw.mayGoLive ? (
                <button
                  type="button"
                  className="acct-open acct-live"
                  disabled={!gw.connected}
                  onClick={() => {
                    if (
                      window.confirm(
                        'Open a LIVE account? Money paid into it is real and at risk.',
                      )
                    ) {
                      gw.openAccount(true)
                    }
                  }}
                >
                  New live account
                </button>
              ) : (
                <button
                  type="button"
                  className="acct-open"
                  disabled={!gw.connected || gw.kyc === 'submitted'}
                  onClick={() => gw.requestVerification()}
                >
                  {gw.kyc === 'submitted'
                    ? 'Verification pending'
                    : gw.kyc === 'rejected'
                      ? 'Ask again to be verified'
                      : 'Verify me for a live account'}
                </button>
              )}
              <p className="acct-note">
                {gw.mayGoLive
                  ? 'You are verified. A live account trades the same way this one does — with real money.'
                  : gw.kyc === 'submitted'
                    ? 'The desk is reviewing your verification. Demo accounts work as usual meanwhile.'
                    : 'A live account needs your identity verified by the desk first. Demo accounts last thirty days and can be renewed.'}
              </p>
            </div>
          </Panel>
        </div>
      </div>

      {/* ── Status bar ── */}
      <div className="statusbar">
        <span className="cap">
          Engine{' '}
          <span style={{ color: statusColour(gw.status) }}>
            {gw.connected ? 'connected' : (gw.detail ?? gw.status)}
          </span>
        </span>
        <span className="cap">
          Journal seq{' '}
          <span className="mono" style={{ color: 'var(--ink-2)' }}>
            {gw.seq.toLocaleString('en-US')}
          </span>
        </span>
        <span className="cap">
          Quote age{' '}
          <span className="mono" style={{ color: 'var(--ink-2)' }}>
            {quote ? `${Math.max(0, Date.now() - quote.at)} ms` : '—'}
          </span>
        </span>
        <span className="cap">
          Source{' '}
          <span className="mono" style={{ color: 'var(--ink-2)' }}>
            {quote?.source ? (SOURCE_NAME[quote.source] ?? quote.source) : gw.connected ? '—' : 'exchange direct'}
          </span>
        </span>
        <div className="grow" />
        <span className="cap">
          {gw.connected ? 'Live prices · demo account · B-book' : 'Exchange prices direct · trading disabled'}
        </span>
      </div>

      {/* ── Deal ticket ── */}
      {ticketOpen ? (
        <div className="ticket">
          <header className="ticket-head">
            <span>New position · {instrument.symbol}</span>
            <button type="button" className="ticket-close" onClick={() => setTicketOpen(false)} aria-label="Close">
              <svg width="11" height="11" viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth={1.5}>
                <path d="M2 2l8 8M10 2l-8 8" />
              </svg>
            </button>
          </header>
          <div className="ticket-body">
            <div className="kinds">
              {(['market', 'limit', 'stop'] as const).map((k) => (
                <button
                  key={k}
                  type="button"
                  className={`kind${k === orderKind ? ' kind-on' : ''}`}
                  onClick={() => chooseKind(k)}
                >
                  {ORDER_TYPE_LABEL[k]}
                </button>
              ))}
            </div>

            {account?.hedging ? (
              <p className="ticket-note">
                Hedging: a sell here opens a short beside your long rather than closing
                it. Close a position from the Trade tab, by its ticket.
              </p>
            ) : null}

            <div className="sides">
              <button
                type="button"
                className="side side-sell"
                disabled={!canSubmit}
                onClick={() => submit('sell')}
              >
                <span className="side-label">SELL</span>
                <span className="mono side-px">{quote ? showPrice(quote.bid, instrument.displayDigits) : '—'}</span>
              </button>
              <button
                type="button"
                className="side side-buy"
                disabled={!canSubmit}
                onClick={() => submit('buy')}
              >
                <span className="side-label">BUY</span>
                <span className="mono side-px">{quote ? showPrice(quote.ask, instrument.displayDigits) : '—'}</span>
              </button>
            </div>

            {viewOnly && listing?.note ? <p className="ticket-note">{listing.note}</p> : null}

            {orderKind !== 'market' ? (
              <label className="px-row">
                <span className="cap">
                  {orderKind === 'limit' ? 'Limit price' : 'Stop price'}
                </span>
                <input
                  className={`px-input mono${restingPrice() === null ? ' px-bad' : ''}`}
                  value={priceText}
                  onChange={(e) => setPriceText(e.target.value)}
                  inputMode="decimal"
                  spellCheck={false}
                  aria-label={orderKind === 'limit' ? 'Limit price' : 'Stop price'}
                />
              </label>
            ) : null}

            <div className="vol-row">
              <span className="cap">Volume</span>
              <div className="stepper">
                <button type="button" onClick={() => adjust(-1n)} aria-label="Decrease volume">
                  &minus;
                </button>
                <span className="mono stepper-value">{showQtyAsLots(volume, contract, 3)}</span>
                <button type="button" onClick={() => adjust(1n)} aria-label="Increase volume">
                  +
                </button>
              </div>
            </div>

            <div className="costs">
              <CostRow label="Notional" value={ticketNotional ? `${showMoney(ticketNotional)} USD` : '—'} />
              <CostRow label="Margin required" value={ticketMargin ? `${showMoney(ticketMargin)} USD` : '—'} />
              <CostRow
                label="Spread"
                value={quote ? showPrice((quote.ask - quote.bid) as Price, instrument.displayDigits) : '—'}
              />
              <CostRow
                label="Free margin after"
                value={
                  account && ticketMargin
                    ? showMoney((account.freeMargin - ticketMargin) as Money)
                    : '—'
                }
              />
            </div>

            {!gw.connected ? (
              <span className="cap" style={{ textAlign: 'center' }}>
                Start the gateway to trade
              </span>
            ) : orderKind !== 'market' ? (
              <span className="cap" style={{ textAlign: 'center' }}>
                Rests until the price is reached. Margin is checked then, not now.
              </span>
            ) : null}
          </div>
        </div>
      ) : null}

      {wallet !== null && account ? (
        <Sheet
          title="Your money"
          note="Nothing here moves money by itself. A person at the desk checks every movement and the account changes then."
          onClose={() => setWallet(null)}
        >
          <Wallet
            methods={gw.methods}
            payouts={gw.payouts}
            mine={gw.myDeposits}
            account={account.id}
            live={account.live}
            accounts={gw.session?.accounts ?? []}
            balance={account.balance}
            credit={account.credit}
            freeMargin={account.freeMargin}
            insurance={account.insurance}
            start={wallet}
            onDeposit={(method, amount, reference) => {
              gw.requestDeposit(method, amount, reference)
              window.setTimeout(() => gw.paymentMethods(), 250)
            }}
            onWithdraw={(payout, amount, note) => {
              gw.requestWithdrawal(payout, amount, note)
              window.setTimeout(() => gw.paymentMethods(), 250)
            }}
            onMove={(to, amount) => {
              gw.transfer(to, amount)
            }}
            onSavePayout={(m) => {
              gw.savePayoutMethod(m)
              window.setTimeout(() => gw.payoutMethods(), 200)
            }}
            onForgetPayout={(id) => {
              gw.forgetPayoutMethod(id)
              window.setTimeout(() => gw.payoutMethods(), 200)
            }}
          />
        </Sheet>
      ) : null}

      <Toasts notices={notices} onDismiss={dismiss} />
    </div>
  )
}

/**
 * One editable protection level on an open position.
 *
 * Click to type, Enter to send, Escape to abandon. An empty box removes the
 * level, which is the only way to say "no stop" that a trader would guess.
 * The engine is the judge of whether a level is allowed — it refuses one on
 * the wrong side of the market — so this does not pre-validate and leave the
 * two disagreeing about what was set.
 */
/** Turns an engine event into something the trader reads. */
function toNotice(event: GatewayEvent, index: number, instruments: readonly Instrument[]): Notice {
  switch (event.kind) {
    case 'filled':
      return {
        id: `fill-${event.ticket}`,
        kind: 'filled',
        title: 'Order filled',
        detail: `${event.side === 'buy' ? 'Bought' : 'Sold'} ${showQtyAsLots(event.qty, contractOf(instruments, event.instrumentId), 3)} ${symbolOf(instruments, event.instrumentId)} at ${showPrice(event.price, digitsOf(instruments, event.instrumentId))} — ticket ${event.ticket}`,
        age: 'now',
        sticky: false,
      }
    case 'rejected': {
      const copy = rejectionCopy(event.reason as RejectReason)
      return {
        id: `reject-${index}-${event.reason}`,
        kind: 'rejected',
        title: copy.title,
        detail: copy.detail,
        age: 'now',
        sticky: true,
      }
    }
    case 'marginCall':
      return {
        id: `call-${event.levelBp}`,
        kind: 'warning',
        title: `Margin level ${showMarginLevel(event.levelBp)}`,
        detail: 'Below the call level. Add funds or reduce exposure before it reaches the stop-out.',
        age: 'now',
        sticky: true,
        action: 'Deposit funds',
      }
    case 'stoppedOut':
      return {
        id: `stopout-${index}-${event.instrumentId}`,
        kind: 'rejected',
        title: 'Position closed by the engine',
        detail: `${showQtyAsLots(event.qty, contractOf(instruments, event.instrumentId), 3)} ${symbolOf(instruments, event.instrumentId)} closed at ${showPrice(event.price, digitsOf(instruments, event.instrumentId))} to protect the account.`,
        age: 'now',
        sticky: true,
      }
    case 'orderPlaced':
      return {
        id: `placed-${event.ticket}`,
        kind: 'funds',
        title: 'Order resting',
        detail: `Ticket ${event.ticket} is on the book, waiting for its price.`,
        age: 'now',
        sticky: false,
      }
    case 'orderCancelled':
      return {
        id: `cancelled-${event.ticket}`,
        kind: 'funds',
        title: 'Order cancelled',
        detail: `Ticket ${event.ticket} was withdrawn from the book.`,
        age: 'now',
        sticky: false,
      }
    case 'rollover':
      return {
        id: `rollover-${event.day}`,
        kind: 'funds',
        title: 'Overnight financing',
        detail:
          event.total === 0n
            ? 'Rollover complete. Nothing was held overnight.'
            : `Rollover complete. ${showSigned(event.total)} accrued on open positions.`,
        age: 'now',
        sticky: false,
      }
    case 'protected':
      return {
        id: `protected-${event.ticket}`,
        kind: event.protection === 'takeProfit' ? 'filled' : 'warning',
        title: event.protection === 'takeProfit' ? 'Take profit hit' : 'Stop loss hit',
        detail: `${showQtyAsLots(event.qty, contractOf(instruments, event.instrumentId), 3)} ${symbolOf(instruments, event.instrumentId)} closed at ${showPrice(event.price, digitsOf(instruments, event.instrumentId))} — ticket ${event.ticket}.`,
        age: 'now',
        sticky: true,
      }
    case 'amended':
      return {
        id: `amended-${event.ticket}-${index}`,
        kind: 'funds',
        title: 'A trade was put right',
        detail:
          event.amendment === 'adjust'
            ? `${showSigned(event.amount ?? (0n as Money))} on ticket ${event.ticket} — ${amendWords(event.reason)}. Raised by the trading desk.`
            : `Ticket ${event.ticket} is now treated as opened at ${showPrice(event.to ?? (0n as Price), 2)}, was ${showPrice(event.from ?? (0n as Price), 2)} — ${amendWords(event.reason)}.`,
        age: 'now',
        // Sticky: a change somebody else made to your trade is not a
        // notification to miss.
        sticky: true,
      }
    case 'protectionSet':
      return {
        id: `protection-${event.ticket}-${event.stopLoss ?? 0}-${event.takeProfit ?? 0}`,
        kind: 'funds',
        title:
          event.stopLoss === null && event.takeProfit === null
            ? 'Protection removed'
            : 'Protection set',
        detail:
          event.stopLoss === null && event.takeProfit === null
            ? `Ticket ${event.ticket} has no stop loss or take profit.`
            : `Ticket ${event.ticket}: ${[
                event.stopLoss === null ? null : `stop ${showPrice(event.stopLoss, 2)}`,
                event.takeProfit === null ? null : `target ${showPrice(event.takeProfit, 2)}`,
              ]
                .filter(Boolean)
                .join(', ')}.`,
        age: 'now',
        sticky: false,
      }
    case 'unauthorized':
      return {
        id: `unauthorized-${index}`,
        kind: 'rejected',
        title: 'Not allowed',
        detail: `${event.detail.charAt(0).toUpperCase()}${event.detail.slice(1)}.`,
        age: 'now',
        sticky: true,
      }
    case 'orderTriggered':
      return {
        id: `triggered-${event.ticket}`,
        kind: 'filled',
        title: 'Order triggered',
        detail: `Ticket ${event.ticket} reached ${showPrice(event.price, 2)}.`,
        age: 'now',
        sticky: false,
      }
  }
}

/** How long a demo account has left, in a trader's words. */
function demoDaysLeft(expiresAt: number): string {
  const left = expiresAt - Date.now()
  if (left <= 0) return 'expired'
  const days = Math.ceil(left / (24 * 60 * 60 * 1000))
  return days === 1 ? '1 day left' : `${days} days left`
}

const STAMP = new Intl.DateTimeFormat('en-GB', {
  day: '2-digit',
  month: 'short',
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
})

const stamp = (ms: number) => (ms > 0 ? STAMP.format(new Date(ms)) : '—')

/** Statement movements in words a trader reads. */
function movementWords(kind: StatementLine['kind']): string {
  switch (kind) {
    case 'deposit':
      return 'Deposit'
    case 'withdrawal':
      return 'Withdrawal'
    case 'bonus':
      return 'Bonus'
    case 'trade':
      return 'Closed trade'
    case 'financing':
      return 'Financing'
    case 'stopOut':
      return 'Stopped out'
    case 'stopLoss':
      return 'Stop loss'
    case 'takeProfit':
      return 'Take profit'
    default:
      return 'Adjustment'
  }
}

/** Amendment reasons in words a trader reads. */
function amendWords(reason: AmendReason): string {
  switch (reason) {
    case 'badPrice':
      return 'the price was not the market\u2019s'
    case 'dealingError':
      return 'a dealing error'
    case 'feeReturned':
      return 'a charge returned'
    case 'goodwill':
      return 'goodwill'
    default:
      return 'a correction'
  }
}


const symbolOf = (list: readonly Instrument[], id: number) =>
  list.find((i) => i.id === id)?.symbol ?? `#${id}`

const digitsOf = (list: readonly Instrument[], id: number) =>
  list.find((i) => i.id === id)?.displayDigits ?? 2

/** Units per lot. One when unknown, so a size is never shown smaller than it is. */
const contractOf = (list: readonly Instrument[], id: number): Qty =>
  list.find((i) => i.id === id)?.contractSize ?? qty('1')

/** Entry lines for whatever is open on the charted symbol. */
function positionLines(
  positions: readonly GatewayPosition[],
  instrument: Instrument,
): readonly PriceLine[] {
  return positions
    .filter((p) => p.instrumentId === instrument.id)
    .map((p) => ({
      price: p.openPrice,
      label: `${p.side.toUpperCase()} ${showQtyAsLots(p.qty, instrument.contractSize, 3)} @ ${showPrice(p.openPrice, instrument.displayDigits)}`,
      tone: p.side,
    }))
}

function dotFor(state: FeedState): string {
  if (state === 'live') return 'ok'
  if (state === 'failed') return 'bad'
  return 'wait'
}

function statusColour(state: FeedState): string {
  if (state === 'live') return 'var(--profit)'
  if (state === 'failed') return 'var(--danger)'
  return 'var(--warn)'
}

function ToolGroup({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div className="tool-group">
      <div className="tool-row">{children}</div>
      <span className="tool-group-name">{name}</span>
    </div>
  )
}

function ToolButton({ label, tone = 'var(--ink-2)', onClick }: { label: string; tone?: string; onClick?: () => void }) {
  return (
    <button type="button" className="tool" onClick={onClick}>
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke={tone} strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round">
        <rect x="2.5" y="2.5" width="19" height="19" />
        <path d="M12 7v10M7 12h10" />
      </svg>
      <span>
        {label.split('\n').map((line, i) => (
          <span key={i} style={{ display: 'block' }}>
            {line}
          </span>
        ))}
      </span>
    </button>
  )
}

function CostRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="cost-row">
      <span className="cap">{label}</span>
      <span className="mono" style={{ fontSize: 11 }}>
        {value}
      </span>
    </div>
  )
}
