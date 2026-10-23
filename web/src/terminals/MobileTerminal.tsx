/**
 * The terminal on a phone.
 *
 * # Why this is a separate screen rather than the desktop one reflowed
 *
 * The desk screen is four panes seen at once, and the whole point of it is
 * that you can. A phone cannot show four panes, so reflowing it produces a
 * column of squashed panels that is worse than either. What a phone is good
 * at is one thing at a time with a way to switch — so this is five screens
 * and a tab bar, and each one is laid out for a thumb.
 *
 * The data is identical: the same socket, the same engine figures, the same
 * refusals. Only the arrangement differs.
 *
 * # The screens
 *
 * | | |
 * |---|---|
 * | **Quotes** | a card per symbol: sell and buy, the spread, where it has been |
 * | **Chart** | the shared chart pane, full width |
 * | **Trades** | what is open, with select-and-close |
 * | **History** | the statement, by period, with totals |
 * | **Net Trade** | one line per symbol: which way, and by how much |
 *
 * The balance bar sits above the tabs on every screen, collapsed to the one
 * figure everybody checks and expanded to the seven that qualify it.
 */

import { useEffect, useMemo, useState } from 'react'
import { Wallet } from '../components/Wallet'
import type { WalletTab } from '../components/Wallet'
import { ChartPane } from '../components/ChartPane'
import { useTape, sparkPath } from '../feed/useTape'
import type { Trail } from '../feed/useTape'
import type { GatewayPosition, StatementLine } from '../feed/gateway'
import type { Interval } from '../feed/types'
import type { GatewayView } from '../feed/useGateway'
import { useChimes } from '../feed/useChimes'
import { wake } from '../lib/chime'
import type { Instrument, Side } from '../lib/domain'
import { DEFAULT_INSTRUMENT, INSTRUMENTS } from '../lib/instruments'
import type { Money, Qty } from '../lib/money'
import {
  qty,
  showMarginLevel,
  showMoney,
  showPrice,
  showQtyAsLots,
  showSigned,
} from '../lib/money'
import './MobileTerminal.css'

type Screen = 'Quotes' | 'Chart' | 'Trades' | 'History' | 'Net Trade'

const SCREENS: readonly Screen[] = ['Quotes', 'Chart', 'Trades', 'History', 'Net Trade']

/** How far back History looks. `all` asks for everything the window holds. */
type Period = 'Day' | 'Week' | 'Month' | 'All'

const SPAN_MS: Record<Period, number> = {
  Day: 86_400_000,
  Week: 7 * 86_400_000,
  Month: 30 * 86_400_000,
  All: Number.POSITIVE_INFINITY,
}

export interface MobileTerminalProps {
  readonly gw: GatewayView
  onTheme(): void
}

export function MobileTerminal({ gw, onTheme }: MobileTerminalProps) {
  const [screen, setScreen] = useState<Screen>('Quotes')
  const [drawer, setDrawer] = useState(false)
  const [open, setOpen] = useState(false)
  const [find, setFind] = useState('')
  const [symbol, setSymbol] = useState(DEFAULT_INSTRUMENT.id)
  const [interval, setInterval] = useState<Interval>('5m')
  const [stars, setStars] = useState<ReadonlySet<number>>(new Set())
  const [ticked, setTicked] = useState<ReadonlySet<number>>(new Set())
  const [period, setPeriod] = useState<Period>('Day')
  /**
   * Which side of the wallet is open, or null for closed.
   *
   * A full-height sheet rather than a sixth tab. Money is an errand, not a
   * place: it is done and left, and a tab bar with six entries on a phone
   * is five targets too small to hit.
   */
  const [wallet, setWallet] = useState<WalletTab | null>(null)

  const instruments: readonly Instrument[] = gw.instruments.length
    ? gw.instruments
    : INSTRUMENTS
  const instrument = instruments.find((i) => i.id === symbol) ?? DEFAULT_INSTRUMENT
  const quote = gw.quotes.get(instrument.id)
  const tape = useTape(gw.quotes)

  // Fills, stop-outs, margin calls and protections, on this account.
  useChimes(gw.events, gw.positions)
  useEffect(() => {
    if (wallet === null) return
    gw.paymentMethods()
    gw.payoutMethods()
  }, [wallet, gw])

  const account = gw.account

  useEffect(() => {
    if (screen === 'History') gw.requestStatement()
  }, [screen, gw])

  // A tick is about a position; it does not follow you to another screen.
  useEffect(() => setTicked(new Set()), [screen])

  const listed = useMemo(() => {
    const needle = find.trim().toUpperCase()
    const matched = needle
      ? instruments.filter((i) => i.symbol.toUpperCase().includes(needle))
      : instruments
    // Starred first: a watch list is what somebody chose to watch.
    return [...matched].sort(
      (a, b) => Number(stars.has(b.id)) - Number(stars.has(a.id)) || a.id - b.id,
    )
  }, [instruments, find, stars])

  const star = (id: number) =>
    setStars((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })

  const chosen = useMemo(
    () => gw.positions.filter((p) => ticked.has(p.ticket)),
    [gw.positions, ticked],
  )

  const openPrice = useMemo(
    () => gw.positions.reduce((acc, p) => acc + p.profit, 0n) as Money,
    [gw.positions],
  )
  const openSwap = useMemo(
    () => gw.positions.reduce((acc, p) => acc + p.swap, 0n) as Money,
    [gw.positions],
  )

  /** One line per symbol: which way this account is, and by how much. */
  const net = useMemo(() => {
    const by = new Map<number, { long: Qty; short: Qty; profit: Money }>()
    for (const p of gw.positions) {
      const at = by.get(p.instrumentId) ?? {
        long: 0n as Qty,
        short: 0n as Qty,
        profit: 0n as Money,
      }
      by.set(p.instrumentId, {
        long: (p.side === 'buy' ? at.long + p.qty : at.long) as Qty,
        short: (p.side === 'sell' ? at.short + p.qty : at.short) as Qty,
        profit: (at.profit + p.profit) as Money,
      })
    }
    return [...by.entries()].map(([id, held]) => ({
      id,
      ...held,
      net: (held.long - held.short) as Qty,
    }))
  }, [gw.positions])

  const history = useMemo(() => {
    const floor = Date.now() - SPAN_MS[period]
    const needle = find.trim().toUpperCase()
    return gw.statement
      .filter((l) => l.at >= floor)
      .filter((l) => needle === '' || String(l.ticket ?? '').includes(needle))
  }, [gw.statement, period, find])

  const deal = (way: Side, inst: Instrument) => {
    const text = window.prompt(
      `${way === 'buy' ? 'Buy' : 'Sell'} how many ${inst.symbol}?`,
      '0.01',
    )
    if (text === null) return
    let volume: Qty
    try {
      volume = qty(text.trim())
    } catch {
      return
    }
    if (volume <= 0n) return
    gw.placeOrder({ instrumentId: inst.id, side: way, kind: 'market', qty: volume })
  }

  return (
    <div className="mob" onPointerDownCapture={wake}>
      {/* ── Top ── */}
      <header className="mob-top">
        <button
          type="button"
          className="mob-round"
          aria-label="Menu"
          onClick={() => setDrawer(true)}
        >
          ☰
        </button>
        <span className="mob-where">{screen}</span>
        <span className={`mob-live${gw.connected ? ' on' : ''}`}>
          {gw.connected ? 'LIVE' : 'OFFLINE'}
        </span>
      </header>

      {screen !== 'Chart' ? (
        <div className="mob-find">
          <span aria-hidden="true">⌕</span>
          <input
            value={find}
            onChange={(e) => setFind(e.target.value)}
            placeholder={screen === 'History' ? 'Ticket number' : 'Symbol name'}
            spellCheck={false}
            autoCapitalize="off"
            aria-label="Search"
          />
        </div>
      ) : null}

      {screen === 'History' ? (
        <div className="mob-periods">
          {(['Day', 'Week', 'Month', 'All'] as const).map((p) => (
            <button
              key={p}
              type="button"
              className={p === period ? 'on' : undefined}
              onClick={() => setPeriod(p)}
            >
              {p}
            </button>
          ))}
        </div>
      ) : null}

      {/* ── The screen ── */}
      <main className="mob-body">
        {screen === 'Quotes'
          ? listed.map((inst) => (
              <QuoteCard
                key={inst.id}
                instrument={inst}
                quote={gw.quotes.get(inst.id)}
                trail={tape.get(inst.id)}
                starred={stars.has(inst.id)}
                onStar={() => star(inst.id)}
                onChart={() => {
                  setSymbol(inst.id)
                  setScreen('Chart')
                }}
                onDeal={(way) => deal(way, inst)}
              />
            ))
          : null}

        {screen === 'Chart' ? (
          <div className="mob-chart">
            <div className="mob-symbols">
              {instruments.map((i) => (
                <button
                  key={i.id}
                  type="button"
                  className={i.id === symbol ? 'on' : undefined}
                  onClick={() => setSymbol(i.id)}
                >
                  {i.symbol}
                </button>
              ))}
            </div>
            <ChartPane
              gw={gw}
              instrument={instrument}
              interval={interval}
              onInterval={setInterval}
              last={quote?.bid}
            />
            <div className="mob-deal">
              <button type="button" className="mob-sell" onClick={() => deal('sell', instrument)}>
                SELL
                <span className="mono">
                  {quote ? showPrice(quote.bid, instrument.displayDigits) : '—'}
                </span>
              </button>
              <button type="button" className="mob-buy" onClick={() => deal('buy', instrument)}>
                BUY
                <span className="mono">
                  {quote ? showPrice(quote.ask, instrument.displayDigits) : '—'}
                </span>
              </button>
            </div>
          </div>
        ) : null}

        {screen === 'Trades' ? (
          <>
            <div className="mob-bulk">
              <label>
                <input
                  type="checkbox"
                  checked={gw.positions.length > 0 && chosen.length === gw.positions.length}
                  disabled={gw.positions.length === 0}
                  onChange={() =>
                    setTicked(
                      chosen.length === gw.positions.length
                        ? new Set()
                        : new Set(gw.positions.map((p) => p.ticket)),
                    )
                  }
                />
                Select all
              </label>
              <button
                type="button"
                disabled={chosen.length === 0}
                onClick={() => {
                  if (!window.confirm(`Close ${chosen.length} position(s)?`)) return
                  for (const p of chosen) gw.closePosition(p.ticket)
                  setTicked(new Set())
                }}
              >
                Close selected
              </button>
            </div>
            {gw.positions.length === 0 ? (
              <p className="mob-empty">Nothing open.</p>
            ) : (
              gw.positions.map((p) => (
                <TradeCard
                  key={p.ticket}
                  position={p}
                  instrument={instruments.find((i) => i.id === p.instrumentId) ?? instrument}
                  ticked={ticked.has(p.ticket)}
                  onTick={() =>
                    setTicked((prev) => {
                      const next = new Set(prev)
                      if (next.has(p.ticket)) next.delete(p.ticket)
                      else next.add(p.ticket)
                      return next
                    })
                  }
                  onClose={() => {
                    if (window.confirm(`Close ticket ${p.ticket}?`)) gw.closePosition(p.ticket)
                  }}
                />
              ))
            )}
          </>
        ) : null}

        {screen === 'History' ? (
          history.length === 0 ? (
            <p className="mob-empty">Nothing in this period.</p>
          ) : (
            history.map((line) => (
              <HistoryCard
                key={line.seq}
                line={line}
                instruments={instruments}
                positions={gw.positions}
              />
            ))
          )
        ) : null}

        {screen === 'Net Trade' ? (
          net.length === 0 ? (
            <p className="mob-empty">Flat.</p>
          ) : (
            net.map((row) => {
              const inst = instruments.find((i) => i.id === row.id) ?? instrument
              const contract = inst.contractSize
              return (
                <div key={row.id} className="mob-net">
                  <div className="mob-net-head">
                    <span className="sym">{inst.symbol}</span>
                    <span className={row.net === 0n ? 'dim' : row.net > 0n ? 'buy' : 'sell'}>
                      {row.net === 0n ? 'FLAT' : row.net > 0n ? 'BUY' : 'SELL'}
                    </span>
                  </div>
                  <div className="mob-net-body">
                    <span className="dim">
                      Net{' '}
                      <span className="mono">
                        {showQtyAsLots((row.net < 0n ? -row.net : row.net) as Qty, contract, 3)}
                      </span>
                    </span>
                    <span className={`mono ${row.profit < 0n ? 'loss' : 'profit'}`}>
                      {showSigned(row.profit)}
                    </span>
                  </div>
                </div>
              )
            })
          )
        ) : null}
      </main>

      {/* ── Balance ── */}
      <div className={`mob-balance${open ? ' open' : ''}`}>
        <button type="button" className="mob-balance-bar" onClick={() => setOpen((o) => !o)}>
          <span>{screen === 'History' ? 'Totals' : 'Balance'}</span>
          <span className="mob-caret" aria-hidden="true">
            {open ? '⌄' : '⌃'}
          </span>
          <span className="mono mob-balance-big">
            {account ? showMoney(account.balance) : '—'}
          </span>
        </button>

        {open ? (
          screen === 'History' ? (
            <Grid
              rows={totalsOf(history)}
            />
          ) : (
            <Grid
              rows={[
                ['Floating P/L', showSigned(openPrice)],
                ['Equity', account ? showMoney(account.equity) : '—'],
                ['Margin level', account ? showMarginLevel(account.marginLevelBp) : '—'],
                ['Used margin', account ? showMoney(account.usedMargin) : '—'],
                ['Interest', showSigned(openSwap)],
                ['Free margin', account ? showMoney(account.freeMargin) : '—'],
                ['Credit', account ? showMoney(account.credit) : '—'],
                ['Account', String(gw.session?.account ?? '—')],
              ]}
            />
          )
        ) : null}
      </div>

      {/* ── Tabs ── */}
      <nav className="mob-tabs">
        {SCREENS.map((s) => (
          <button
            key={s}
            type="button"
            className={s === screen ? 'on' : undefined}
            onClick={() => {
              setScreen(s)
              setFind('')
            }}
          >
            <span className="mob-tab-icon" aria-hidden="true">
              {TAB_ICON[s]}
            </span>
            {s}
          </button>
        ))}
      </nav>

      {/* ── Money ── */}
      {wallet !== null && account ? (
        <div className="mob-money">
          <header className="mob-money-head">
            <button
              type="button"
              className="mob-money-back"
              onClick={() => setWallet(null)}
              aria-label="Back"
            >
              ‹
            </button>
            <span>Your money</span>
            <span className="mob-money-acct mono">{account.id}</span>
          </header>
          <div className="mob-money-body">
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
          </div>
        </div>
      ) : null}

      {/* ── Drawer ── */}
      {drawer ? (
        <div className="mob-drawer-back" onClick={() => setDrawer(false)}>
          <aside className="mob-drawer" onClick={(e) => e.stopPropagation()}>
            <header className="mob-drawer-head">
              <span className="mob-avatar">
                {(gw.session?.login ?? 'F').slice(0, 1).toUpperCase()}
              </span>
              <div>
                <div className="mob-drawer-name">{gw.session?.login}</div>
                <div className="mob-drawer-sub">
                  {gw.session?.role} · {gw.session?.account}
                  {account?.live ? ' · LIVE' : ' · DEMO'}
                </div>
              </div>
            </header>

            <div className="mob-drawer-list">
              {SCREENS.map((s) => (
                <button
                  key={s}
                  type="button"
                  onClick={() => {
                    setScreen(s)
                    setDrawer(false)
                  }}
                >
                  <span aria-hidden="true">{TAB_ICON[s]}</span>
                  {s}
                </button>
              ))}

              <div className="mob-drawer-rule" />

              <button
                type="button"
                onClick={() => {
                  setWallet('in')
                  setDrawer(false)
                }}
              >
                <span aria-hidden="true">↓</span>
                Pay in
              </button>
              <button
                type="button"
                onClick={() => {
                  setWallet('out')
                  setDrawer(false)
                }}
              >
                <span aria-hidden="true">↑</span>
                Withdraw
              </button>

              <div className="mob-drawer-rule" />

              {(gw.session?.accounts ?? []).map((a) => (
                <button
                  key={a.id}
                  type="button"
                  onClick={() => {
                    gw.selectAccount(a.id)
                    setDrawer(false)
                  }}
                >
                  <span aria-hidden="true">{a.id === gw.session?.account ? '◉' : '○'}</span>
                  {a.id} · {a.live ? 'live' : 'demo'}
                </button>
              ))}

              {gw.session?.role === 'client' ? (
                <button
                  type="button"
                  onClick={() => {
                    gw.openAccount(false)
                    setDrawer(false)
                  }}
                >
                  <span aria-hidden="true">＋</span>
                  Open a demo account
                </button>
              ) : null}

              <div className="mob-drawer-rule" />

              <button type="button" onClick={onTheme}>
                <span aria-hidden="true">◐</span>
                Light or dark
              </button>
              <button type="button" className="danger" onClick={gw.signOut}>
                <span aria-hidden="true">⏻</span>
                Sign out
              </button>
            </div>
          </aside>
        </div>
      ) : null}
    </div>
  )
}

const TAB_ICON: Record<Screen, string> = {
  Quotes: '⇅',
  Chart: '▥',
  Trades: '◎',
  History: '☷',
  'Net Trade': '∑',
}

/** The two-column figure grid under the balance bar. */
function Grid({ rows }: { rows: readonly (readonly [string, string])[] }) {
  return (
    <div className="mob-grid">
      {rows.map(([label, value]) => (
        <div key={label} className="mob-cell">
          <span>{label}</span>
          <span className="mono">{value}</span>
        </div>
      ))}
    </div>
  )
}

/**
 * What the statement adds up to over the chosen period.
 *
 * Summed from the lines on screen rather than asked for separately, so the
 * total always agrees with the list above it — a total that disagrees with
 * its own rows is worse than no total.
 */
function totalsOf(lines: readonly StatementLine[]): readonly (readonly [string, string])[] {
  const sum = (kinds: readonly string[]) =>
    lines.filter((l) => kinds.includes(l.kind)).reduce((acc, l) => acc + l.amount, 0n) as Money
  return [
    ['Profit', showSigned(sum(['trade', 'stopOut', 'stopLoss', 'takeProfit']))],
    ['Deposits', showSigned(sum(['deposit']))],
    ['Interest', showSigned(sum(['financing']))],
    ['Withdrawals', showSigned(sum(['withdrawal']))],
    ['Bonus', showSigned(sum(['bonus']))],
    ['Adjustments', showSigned(sum(['adjustment']))],
    // Both legs, so this reads zero when a client only moved money between
    // their own accounts — which is the honest total: they are no better
    // or worse off. It is non-zero only when one leg is outside the window.
    ['Moved', showSigned(sum(['transferIn', 'transferOut']))],
  ]
}

/** One symbol on the Quotes screen. */
function QuoteCard({
  instrument,
  quote,
  trail,
  starred,
  onStar,
  onChart,
  onDeal,
}: {
  instrument: Instrument
  quote: ReturnType<GatewayView['quotes']['get']>
  trail: Trail | undefined
  starred: boolean
  onStar(): void
  onChart(): void
  onDeal(way: Side): void
}) {
  const d = instrument.displayDigits
  const spread = quote && instrument.tickSize > 0n ? (quote.ask - quote.bid) / instrument.tickSize : null
  const path = sparkPath(trail, 92, 38)

  return (
    <article className="mob-quote">
      <header>
        <button
          type="button"
          className={`mob-star${starred ? ' on' : ''}`}
          aria-label={starred ? 'Unpin' : 'Pin'}
          onClick={onStar}
        >
          {starred ? '★' : '☆'}
        </button>
        <span className="dim">Spread {spread === null ? '—' : String(spread)}</span>
        <button type="button" className="mob-quote-sym" onClick={onChart}>
          {instrument.symbol}
        </button>
        <span className="dim mob-quote-src">{quote?.source ?? ''}</span>
      </header>

      <div className="mob-quote-prices">
        <span className="mono sell">{quote ? showPrice(quote.bid, d) : '—'}</span>
        <span className="mono buy">{quote ? showPrice(quote.ask, d) : '—'}</span>
      </div>

      <div className="mob-quote-deal">
        <button type="button" className="mob-sell" onClick={() => onDeal('sell')}>
          SELL
        </button>
        <svg className="mob-spark" viewBox="0 0 92 38" aria-hidden="true">
          {path ? (
            <>
              <path d={`${path} L92,38 L0,38 Z`} className="mob-spark-fill" />
              <path d={path} className="mob-spark-line" />
            </>
          ) : null}
        </svg>
        <button type="button" className="mob-buy" onClick={() => onDeal('buy')}>
          BUY
        </button>
      </div>

      <footer>
        <span className="mob-pip low">L</span>
        <span className="mono dim">{trail ? showPrice(trail.low, d) : '—'}</span>
        <span className="mob-since dim">
          {trail ? 'since you opened this page' : 'waiting for a price'}
        </span>
        <span className="mono dim">{trail ? showPrice(trail.high, d) : '—'}</span>
        <span className="mob-pip high">H</span>
      </footer>
    </article>
  )
}

/** One open position on the Trades screen. */
function TradeCard({
  position,
  instrument,
  ticked,
  onTick,
  onClose,
}: {
  position: GatewayPosition
  instrument: Instrument
  ticked: boolean
  onTick(): void
  onClose(): void
}) {
  const d = instrument.displayDigits
  return (
    <article className="mob-trade">
      <input type="checkbox" checked={ticked} onChange={onTick} aria-label={`Select ${position.ticket}`} />
      <div className="mob-trade-body">
        <div className="mob-trade-head">
          <span className="sym">{instrument.symbol}</span>
          <span className={position.side === 'buy' ? 'buy' : 'sell'}>
            {position.side === 'buy' ? 'BUY' : 'SELL'}
          </span>
        </div>
        <div className="dim mob-trade-line">
          Amount <span className="mono">{showQtyAsLots(position.qty, instrument.contractSize, 3)}</span>
          {' · '}
          <span className="mono">{showPrice(position.openPrice, d)}</span>
          {' → '}
          <span className="mono">
            {position.current ? showPrice(position.current, d) : '—'}
          </span>
        </div>
      </div>
      <div className="mob-trade-right">
        <span className={`mob-pnl ${position.profit < 0n ? 'loss' : 'profit'}`}>
          {showSigned(position.profit)}
        </span>
        <button type="button" onClick={onClose}>
          Close
        </button>
      </div>
    </article>
  )
}

/** One statement line on the History screen. */
function HistoryCard({
  line,
  instruments,
  positions,
}: {
  line: StatementLine
  instruments: readonly Instrument[]
  positions: readonly GatewayPosition[]
}) {
  const held = positions.find((p) => p.ticket === line.ticket)
  const inst = held ? instruments.find((i) => i.id === held.instrumentId) : undefined
  return (
    <article className="mob-hist">
      <div>
        <div className="mob-hist-what">{WORDS[line.kind] ?? line.kind}</div>
        <div className="dim mob-hist-when">
          {WHEN.format(new Date(line.at))}
          {line.ticket === null ? '' : ` · ticket ${line.ticket}`}
          {inst ? ` · ${inst.symbol}` : ''}
        </div>
      </div>
      <div className="mob-hist-right">
        <span className={`mono ${line.amount < 0n ? 'loss' : 'profit'}`}>
          {showSigned(line.amount)}
        </span>
        <span className="mono dim">{showMoney(line.balance)}</span>
      </div>
    </article>
  )
}

const WORDS: Record<string, string> = {
  deposit: 'Deposit',
  withdrawal: 'Withdrawal',
  bonus: 'Bonus',
  trade: 'Trade closed',
  financing: 'Overnight financing',
  stopOut: 'Closed to protect the account',
  stopLoss: 'Stop loss',
  takeProfit: 'Take profit',
  adjustment: 'Adjustment by the desk',
}

const WHEN = new Intl.DateTimeFormat(undefined, {
  day: '2-digit',
  month: '2-digit',
  year: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})
