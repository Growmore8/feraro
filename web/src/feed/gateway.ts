/**
 * Client for the Feraro gateway.
 *
 * The gateway is the authority: it lists the instruments, owns the prices, and
 * computes every account figure. The terminal renders what it is told and
 * sends orders back. Nothing here calculates a balance — if the browser and
 * the engine ever disagree about equity, the browser is wrong by construction.
 *
 * Every monetary value crosses as a decimal string and is parsed into
 * fixed-point here, by the same rules the engine used to produce it. No
 * `JSON.parse` result is ever treated as a price.
 */

import type { Candle } from '../components/PriceChart'
import { hhmm } from './binance'
import type { Instrument, OrderType, RejectReason } from '../lib/domain'
import type { Money, Price, Qty, Side } from '../lib/money'
import { money, price, qty } from '../lib/money'
import type { FeedQuote, FeedState } from './types'
import { backoffMs } from './types'

/**
 * Where the gateway listens.
 *
 * Derived from the page's own origin rather than hard-coded, so one build
 * works on a laptop and on a server. Behind a proxy the socket is the same
 * host and scheme as the page, at `/ws`; in development Vite serves the page
 * on 5173 while the gateway listens on 8081, which is the one case that
 * needs a different port.
 *
 * `VITE_GATEWAY_URL` overrides both, for a deployment that puts the gateway
 * somewhere else entirely.
 *
 * The scheme follows the page's: a `ws://` socket opened from an `https://`
 * page is blocked by the browser, and it should be — a password must not
 * cross a network in clear because one constant was left behind.
 */
export const GATEWAY_URL: string = (() => {
  const told = import.meta.env['VITE_GATEWAY_URL']
  if (typeof told === 'string' && told !== '') return told
  if (typeof window === 'undefined') return 'ws://127.0.0.1:8081'
  const { protocol, hostname, port } = window.location
  const ws = protocol === 'https:' ? 'wss:' : 'ws:'
  // Vite's dev server. Anything else is a real deployment behind a proxy.
  if (port === '5173') return `${ws}//${hostname}:8081`
  return `${ws}//${window.location.host}/ws`
})()

/** An instrument as the gateway lists it, including its order limits. */
export interface GatewayInstrument extends Instrument {
  readonly minQty: Qty
  readonly maxQty: Qty
  readonly qtyStep: Qty
  /** False when the engine refuses orders on it, for the reason in `note`. */
  readonly tradeable: boolean
  readonly note: string | null
}

/** Account figures, all computed by the engine. */
export interface GatewayAccount {
  readonly id: number
  readonly currency: string
  readonly leverage: number
  readonly balance: Money
  readonly credit: Money
  /**
   * The client's own money, set aside where the market cannot reach it.
   *
   * Not the bonus: it supports no margin, so free margin falls by it.
   */
  readonly insurance: Money
  readonly equity: Money
  readonly usedMargin: Money
  readonly freeMargin: Money
  readonly marginLevelBp: number | null
  /**
   * True when both sides of an instrument may be held at once.
   *
   * The deal ticket says so, because it changes what a sell does on an
   * account that is already long: close it, or open a short beside it.
   */
  readonly hedging: boolean
  /** True when the money in it is the client's. */
  readonly live: boolean
  /** When a demo account expires, epoch milliseconds. Null when live. */
  readonly demoExpiresAt: number | null
}

/** One open position, valued by the engine. */
export interface GatewayPosition {
  readonly ticket: number
  readonly instrumentId: number
  readonly side: Side
  readonly qty: Qty
  readonly openPrice: Price
  readonly current: Price | null
  /** Price profit only. */
  readonly profit: Money
  /** Overnight financing accrued and not yet realised. */
  readonly swap: Money
  /** The level that closes it at a loss, when one is set. */
  readonly stopLoss: Price | null
  /** The level that takes the profit, when one is set. */
  readonly takeProfit: Price | null
}

/** One order resting on the book, waiting for its price. */
export interface GatewayPending {
  readonly ticket: number
  readonly instrumentId: number
  readonly side: Side
  readonly kind: OrderType
  readonly qty: Qty
  readonly triggerPrice: Price
  /** Where a stop-limit rests once triggered. */
  readonly limitPrice: Price | null
}

/** Where this connection is in signing in. */
export type AuthStage =
  /** Nothing asked for yet, or the socket is down. */
  | 'unknown'
  /** The gateway wants credentials. */
  | 'required'
  /** Credentials are with the gateway. */
  | 'signingIn'
  /** Signed in; the terminal is live. */
  | 'signedIn'

/** One account as the picker lists it. */
export interface GatewayAccountSummary {
  readonly id: number
  readonly currency: string
  readonly balance: Money
  readonly equity: Money
  readonly owner: number | null
  /** True when the money in it is the client's. */
  readonly live: boolean
  /** When a demo account expires, epoch milliseconds. Null when live. */
  readonly demoExpiresAt: number | null
}

/** Who is signed in, and what they may act on. */
export interface GatewaySession {
  /** Resumes this session on a reload. Never leaves the browser except back
   * to the gateway that issued it. */
  readonly token: string
  readonly user: number
  readonly login: string
  readonly role: Role
  readonly accounts: readonly GatewayAccountSummary[]
  /** The account every command is acting on. */
  readonly account: number
  /** True when this login has a working authenticator. */
  readonly secondFactor: boolean
  /**
   * True when this login can move money and has no authenticator.
   *
   * The terminal says so until it is dealt with. A desk login is the one
   * that can take money out of a client's account, and a password is a
   * single thing somebody can take.
   */
  readonly secondFactorWanted: boolean
}

/** One account as the administrator's book shows it. */
export interface AdminAccount {
  readonly id: number
  readonly owner: number | null
  readonly ownerLogin: string | null
  readonly currency: string
  readonly leverage: number
  readonly balance: Money
  readonly credit: Money
  /** Set aside out of the market's reach, and not margin. */
  readonly insurance: Money
  readonly equity: Money
  readonly usedMargin: Money
  readonly marginLevelBp: number | null
  readonly positions: number
  readonly pending: number
  readonly tradeable: boolean
  /** True when both sides of an instrument may be held at once. */
  readonly hedging: boolean
  /** True for live, false for demo. */
  readonly live: boolean
  /** When a demo account expires, epoch milliseconds. */
  readonly demoExpiresAt: number | null
}

/** One login, as the people list shows it. */
export interface AdminUser {
  readonly id: number
  /** Their email address, which is their login. */
  readonly login: string
  readonly role: Role
  /** How far identity checks have got. */
  readonly kyc: Kyc
  /** The manager whose book this client sits in, when one was assigned. */
  readonly manager: number | null
  /** That manager's login, joined by the gateway so this need not look. */
  readonly managerLogin: string | null
  /**
   * Every power and whether this login holds it.
   *
   * Empty for anybody but a manager: an administrator holds everything by
   * definition and a client holds none of it, so a row of switches
   * against either would do nothing.
   */
  readonly powers: readonly Power[]
}

/** One power a desk login holds, or does not. */
export interface Power {
  /** Stable name, which is what the gateway takes back. */
  readonly key: string
  /** What to call it on screen. */
  readonly said: string
  readonly held: boolean
  /** False when no switch can give it to a manager. */
  readonly possible: boolean
  /** Why not, when it cannot. */
  readonly whyNot: string | null
}

/**
 * What a signed-in user may do.
 *
 * A manager gets the same screen as an administrator over a smaller book —
 * their own clients. The browser uses this to decide what to *show*; the
 * gateway decides what is allowed, and refuses anything else whatever the
 * screen offered.
 */
export type Role = 'client' | 'manager' | 'admin'

export const asRole = (name: string): Role =>
  name === 'admin' ? 'admin' : name === 'manager' ? 'manager' : 'client'

/** How money travels when a client pays in. */
export type Rail = 'crypto' | 'upi' | 'bank'

export const asRail = (name: string): Rail =>
  name === 'crypto' ? 'crypto' : name === 'upi' ? 'upi' : 'bank'

/** A way to pay money in, as the desk has set it up. */
export interface PaymentMethod {
  readonly id: number
  readonly rail: Rail
  readonly label: string
  /** The wallet address, UPI id or bank details. Shown verbatim. */
  readonly address: string
  /** Anything the client must do to be credited. */
  readonly note: string
  /** Off means clients do not see it. Only the desk is sent these. */
  readonly enabled: boolean
}

/** Where a request has got to. */
export type Stage = 'pending' | 'credited' | 'refused'

/**
 * What is being asked for.
 *
 * Every way money moves except a trade, in one queue. The client may raise
 * the first two; the rest are the house's to grant.
 */
export type MoneyKind =
  | 'deposit'
  | 'withdrawal'
  | 'bonusIn'
  | 'bonusOut'
  | 'insuranceIn'
  | 'insuranceOut'

export const MONEY_KINDS: readonly MoneyKind[] = [
  'deposit',
  'withdrawal',
  'bonusIn',
  'bonusOut',
  'insuranceIn',
  'insuranceOut',
]

/** What each is called on screen. */
export const KIND_WORD: Readonly<Record<MoneyKind, string>> = {
  deposit: 'Deposit',
  withdrawal: 'Withdrawal',
  bonusIn: 'Bonus in',
  bonusOut: 'Bonus out',
  insuranceIn: 'Insurance in',
  insuranceOut: 'Insurance out',
}

export const asMoneyKind = (name: string): MoneyKind =>
  (MONEY_KINDS as readonly string[]).includes(name) ? (name as MoneyKind) : 'deposit'

/** Where one client wants their money sent. */
export interface PayoutMethod {
  readonly id: number
  readonly rail: Rail
  readonly label: string
  readonly address: string
  readonly note: string
  /** The one used unless another is chosen. */
  readonly preferred: boolean
}

/** One instrument as the symbol screen shows it. */
export interface SymbolSetting {
  readonly instrument: number
  readonly symbol: string
  /** What it is priced in, so the screen can group by it. */
  readonly quote: string
  readonly displayDigits: number
  readonly tickSize: string
  readonly contractSize: string
  /** The sizes in force — the catalogue's, or the desk's. */
  readonly minQty: string
  readonly maxQty: string
  readonly qtyStep: string
  /** False when it is closed to orders. */
  readonly tradeable: boolean
  /**
   * True when the desk has set the sizes.
   *
   * Shown so a row that is somebody's decision can be told from a row
   * that is just the catalogue — otherwise "put it back" is a button
   * nobody knows whether they need.
   */
  readonly overridden: boolean
  readonly note: string | null
}

/** What the house charges on one instrument. */
export interface Pricing {
  readonly instrument: number
  readonly symbol: string
  /** Added to each side, so the spread widens by twice it. */
  readonly markup: string
  /** Charged per unit when a position opens and when it closes. */
  readonly commission: string
  /**
   * The widened spread as it stands, in the instrument's own points.
   *
   * Null until a price has arrived: a spread is a fact about two live
   * prices, not about the markup on its own.
   */
  readonly spread: string | null
}

/**
 * A client's claim that they have paid.
 *
 * `amount` is what the client says they sent — a claim, not a figure the
 * platform has agreed to — so it stays a string until an operator confirms
 * it. Nothing in the browser should add it up.
 */
export interface DepositRequest {
  readonly id: number
  /** What is being asked for. */
  readonly kind: MoneyKind
  /** Which pot it moves: balance, bonus or insurance. */
  readonly pot: string
  readonly account: number
  readonly user: number
  readonly login: string | null
  readonly method: number
  readonly methodLabel: string
  readonly rail: Rail
  readonly amount: string
  readonly reference: string
  readonly at: number
  readonly stage: Stage
  readonly settledBy: number | null
  readonly detail: string
}

/** How far a client has got with identity checks. */
export type Kyc = 'none' | 'submitted' | 'approved' | 'rejected'

const asKyc = (text: string): Kyc =>
  text === 'submitted' || text === 'approved' || text === 'rejected' ? text : 'none'

/** Bars the gateway has drawn from the prices it has seen. */
export interface GatewayCandles {
  readonly instrumentId: number
  readonly interval: string
  readonly bars: readonly Candle[]
}

/** One line of an account statement. */
export interface StatementLine {
  readonly seq: number
  readonly at: number
  readonly kind:
    | 'deposit'
    | 'withdrawal'
    | 'bonus'
    | 'trade'
    | 'financing'
    | 'stopOut'
    | 'stopLoss'
    | 'takeProfit'
    | 'adjustment'
    /** Money leaving for another of this client's accounts. */
    | 'transferOut'
    /** Money arriving from another of them. */
    | 'transferIn'
  readonly ticket: number | null
  readonly amount: Money
  /** The balance after it. What makes a statement a statement. */
  readonly balance: Money
}

/** One line of the client activity log. */
export interface ActivityLine {
  readonly seq: number
  readonly at: number
  readonly user: number
  readonly login: string | null
  readonly what: 'signedIn' | 'resumed' | 'refused' | 'signedOut'
  /** Where from. Empty when the gateway could not tell. */
  readonly address: string
}

/** One pot in the double-entry books. */
export interface BookLine {
  readonly owner: number
  readonly house: boolean
  /** `ClientCash`, `Bonus`, `RealisedPnl`, `Swap`, `PspClearing` … */
  readonly book: string
  readonly currency: string
  readonly amount: Money
}

/** The books, and what each currency adds up to. */
export interface Books {
  readonly lines: readonly BookLine[]
  /** One per currency. Every residual must be zero. */
  readonly trial: readonly { readonly currency: string; readonly residual: Money }[]
  readonly transactions: number
}

/** One line of the audit trail. */
export interface AuditLine {
  readonly seq: number
  readonly at: number
  readonly by: number
  readonly byLogin: string | null
  readonly account: number
  readonly action: string
  readonly detail: string
}

/** What came of the administrator's last action. */
export interface AdminResult {
  readonly done: boolean
  readonly detail: string
}

/** Everything the gateway has told us. */
export interface GatewayState {
  readonly status: FeedState
  readonly detail?: string
  readonly instruments: readonly GatewayInstrument[]
  readonly quotes: ReadonlyMap<number, FeedQuote>
  readonly account: GatewayAccount | null
  readonly positions: readonly GatewayPosition[]
  readonly pending: readonly GatewayPending[]
  /** Journal position, shown in the status bar. */
  readonly seq: number
  /** How far this connection has got with signing in. */
  readonly auth: AuthStage
  /** Who is signed in, once somebody is. */
  readonly session: GatewaySession | null
  /** Why the last attempt failed, for the login form. */
  readonly authError: string | null
  /** How far the signed-in client's identity checks have got. */
  readonly kyc: Kyc
  /** True when they may open a live account. */
  readonly mayGoLive: boolean
  /** The book, once an administrator has asked for it. */
  readonly book: readonly AdminAccount[]
  /** Logins, for the people list. */
  readonly users: readonly AdminUser[]
  /** The audit trail, newest first. */
  readonly audit: readonly AuditLine[]
  /** The double-entry books, once an administrator has asked for them. */
  readonly books: Books | null
  /** This account's movements, newest first. */
  readonly statement: readonly StatementLine[]
  /** How money may be paid in. */
  readonly methods: readonly PaymentMethod[]
  /** This client's own deposit claims, newest first. */
  readonly myDeposits: readonly DepositRequest[]
  /**
   * True when the sign-in screen should ask for an authenticator code.
   *
   * Set by the gateway rather than guessed: a terminal cannot know which
   * logins have a second factor without being told, and asking everybody
   * would ask the clients too.
   */
  readonly codeWanted: boolean
  /** The secret from a fresh enrolment, which is shown exactly once. */
  readonly secondFactorSecret: {
    readonly secret: string
    readonly grouped: string
    readonly uri: string
  } | null
  /** The desk's queue of requests to decide. */
  readonly deposits: readonly DepositRequest[]
  /** Where this client's money may be sent. */
  readonly payouts: readonly PayoutMethod[]
  /** What the house charges, per instrument. */
  readonly pricing: readonly Pricing[]
  /** Every instrument, and what the desk has changed about it. */
  readonly symbols: readonly SymbolSetting[]
  /** Who signed in, newest first. */
  readonly activity: readonly ActivityLine[]
  /**
   * The last bars the gateway sent.
   *
   * One answer at a time, not a cache per instrument: the chart asks for what
   * it is showing, and keeping every series it ever showed would be a slow
   * memory leak nobody notices until a long session.
   */
  readonly candles: GatewayCandles | null
  /** What came of the last administrative action. */
  readonly adminResult: AdminResult | null
}

/** Something the terminal should tell the trader about. */
export type GatewayEvent =
  | { readonly kind: 'filled'; readonly ticket: number; readonly instrumentId: number; readonly side: Side; readonly qty: Qty; readonly price: Price }
  | { readonly kind: 'rejected'; readonly reason: RejectReason }
  | { readonly kind: 'marginCall'; readonly levelBp: number }
  | { readonly kind: 'stoppedOut'; readonly instrumentId: number; readonly qty: Qty; readonly price: Price }
  | { readonly kind: 'orderPlaced'; readonly ticket: number }
  | { readonly kind: 'orderCancelled'; readonly ticket: number }
  | { readonly kind: 'orderTriggered'; readonly ticket: number; readonly price: Price }
  | { readonly kind: 'rollover'; readonly day: number; readonly total: Money }
  | { readonly kind: 'unauthorized'; readonly detail: string }
  | {
      readonly kind: 'protected'
      readonly ticket: number
      readonly instrumentId: number
      readonly protection: 'stopLoss' | 'takeProfit'
      readonly qty: Qty
      readonly price: Price
    }
  | {
      readonly kind: 'amended'
      readonly ticket: number
      readonly amendment: 'adjust' | 'correctPrice'
      readonly amount: Money | null
      readonly from: Price | null
      readonly to: Price | null
      readonly reason: AmendReason
    }
  | {
      readonly kind: 'protectionSet'
      readonly ticket: number
      readonly stopLoss: Price | null
      readonly takeProfit: Price | null
    }

/** What the client reports back as messages arrive. */
export interface GatewayHandlers {
  onState(patch: Partial<GatewayState>): void
  onEvent(event: GatewayEvent): void
  /** Called when the socket opens, so a stored token can be offered. */
  onOpen?(): void
}

const asSide = (s: string): Side => (s === 'sell' ? 'sell' : 'buy')

function parseInstrument(raw: Record<string, unknown>): GatewayInstrument {
  return {
    id: Number(raw['id']),
    symbol: String(raw['symbol']),
    base: String(raw['base']),
    quote: String(raw['quote']),
    displayDigits: Number(raw['displayDigits']),
    contractSize: qty(String(raw['contractSize'])),
    tickSize: price(String(raw['tickSize'])),
    minQty: qty(String(raw['minQty'])),
    maxQty: qty(String(raw['maxQty'])),
    qtyStep: qty(String(raw['qtyStep'])),
    // An older gateway sends neither; everything it listed was tradeable.
    tradeable: raw['tradeable'] !== false,
    note: typeof raw['note'] === 'string' ? raw['note'] : null,
  }
}

function parseSummary(raw: Record<string, unknown>): GatewayAccountSummary {
  const owner = raw['owner']
  return {
    id: Number(raw['id']),
    currency: String(raw['currency']),
    balance: money(String(raw['balance'])),
    equity: money(String(raw['equity'])),
    owner: owner === null || owner === undefined ? null : Number(owner),
    live: raw['live'] === true,
    demoExpiresAt: optionalNumber(raw['demoExpiresAt']),
  }
}

/** A number the gateway may omit. */
function optionalNumber(raw: unknown): number | null {
  return raw === null || raw === undefined ? null : Number(raw)
}

/** The copy a trader reads when a sign-in does not go through. */
function authMessage(reason: string, retryInSeconds: number | null): string {
  switch (reason) {
    case 'refused':
      // Deliberately not "no such user": which half was wrong is not the
      // trader's business to be told, and not an attacker's to learn.
      return 'That login and password do not match.'
    case 'codeRequired':
      return 'This login needs a code from your authenticator.'
    case 'codeRefused':
      // Not "wrong code": it may have been right and already used, and a
      // person who reads "wrong" will retype the same one.
      return 'That code was not accepted. Wait for the next one and try again.'
    case 'tooManyAttempts':
      return retryInSeconds === null
        ? 'Too many attempts. Try again shortly.'
        : `Too many attempts. Try again in ${retryInSeconds} seconds.`
    case 'expired':
      return 'Your session has ended. Sign in again.'
    case 'noAccounts':
      return 'This login has no trading account yet. Contact support.'
    default:
      return 'Could not sign in.'
  }
}

function parsePower(raw: Record<string, unknown>): Power {
  return {
    key: String(raw['key'] ?? ''),
    said: String(raw['said'] ?? ''),
    held: raw['held'] === true,
    // Absent means possible: a gateway from before this existed had no
    // impossible powers, and reading silence as "forbidden" would grey
    // out every switch on the screen.
    possible: raw['possible'] !== false,
    whyNot: typeof raw['whyNot'] === 'string' ? raw['whyNot'] : null,
  }
}

function parseAdminAccount(raw: Record<string, unknown>): AdminAccount {
  const owner = raw['owner']
  const level = raw['marginLevelBp']
  return {
    id: Number(raw['id']),
    owner: owner === null || owner === undefined ? null : Number(owner),
    ownerLogin: typeof raw['ownerLogin'] === 'string' ? raw['ownerLogin'] : null,
    currency: String(raw['currency']),
    leverage: Number(raw['leverage']),
    balance: money(String(raw['balance'])),
    credit: money(String(raw['credit'])),
    insurance: money(String(raw['insurance'] ?? '0')),
    equity: money(String(raw['equity'])),
    usedMargin: money(String(raw['usedMargin'])),
    marginLevelBp: level === null || level === undefined ? null : Number(level),
    positions: Number(raw['positions'] ?? 0),
    pending: Number(raw['pending'] ?? 0),
    tradeable: raw['tradeable'] !== false,
    hedging: raw['hedging'] === true,
    live: raw['live'] === true,
    demoExpiresAt: optionalNumber(raw['demoExpiresAt']),
  }
}

function parseAuditLine(raw: Record<string, unknown>): AuditLine {
  return {
    seq: Number(raw['seq']),
    at: Number(raw['at']),
    by: Number(raw['by']),
    byLogin: typeof raw['byLogin'] === 'string' ? raw['byLogin'] : null,
    account: Number(raw['account']),
    action: String(raw['action']),
    detail: String(raw['detail'] ?? ''),
  }
}

function parseAccount(raw: Record<string, unknown>): GatewayAccount {
  const level = raw['marginLevelBp']
  return {
    id: Number(raw['id']),
    currency: String(raw['currency']),
    leverage: Number(raw['leverage']),
    balance: money(String(raw['balance'])),
    credit: money(String(raw['credit'])),
    insurance: money(String(raw['insurance'] ?? '0')),
    equity: money(String(raw['equity'])),
    usedMargin: money(String(raw['usedMargin'])),
    freeMargin: money(String(raw['freeMargin'])),
    marginLevelBp: level === null || level === undefined ? null : Number(level),
    hedging: raw['hedging'] === true,
    live: raw['live'] === true,
    demoExpiresAt: optionalNumber(raw['demoExpiresAt']),
  }
}

const asActivity = (text: string): ActivityLine['what'] =>
  text === 'signedIn' || text === 'resumed' || text === 'refused' || text === 'signedOut'
    ? text
    : 'refused'

const asAmendReason = (text: string): AmendReason =>
  text === 'badPrice' ||
  text === 'dealingError' ||
  text === 'feeReturned' ||
  text === 'goodwill'
    ? text
    : 'other'

/** A price the gateway may omit. Absent and zero both mean "none". */
function optionalPrice(raw: unknown): Price | null {
  if (raw === null || raw === undefined) return null
  const parsed = price(String(raw))
  return parsed > 0n ? parsed : null
}

function parsePosition(raw: Record<string, unknown>): GatewayPosition {
  const current = raw['current']
  return {
    ticket: Number(raw['ticket']),
    instrumentId: Number(raw['instrument']),
    side: asSide(String(raw['side'])),
    qty: qty(String(raw['qty'])),
    openPrice: price(String(raw['openPrice'])),
    current: current === null || current === undefined ? null : price(String(current)),
    profit: money(String(raw['profit'])),
    // A gateway older than rollover support sends no swap; that is zero, not
    // a parse failure that blanks the whole blotter.
    swap: money(String(raw['swap'] ?? '0')),
    stopLoss: optionalPrice(raw['stopLoss']),
    takeProfit: optionalPrice(raw['takeProfit']),
  }
}

const asKind = (s: string): OrderType =>
  s === 'limit' || s === 'stop' || s === 'stopLimit' ? s : 'market'

function parsePending(raw: Record<string, unknown>): GatewayPending {
  const limit = raw['limitPrice']
  return {
    ticket: Number(raw['ticket']),
    instrumentId: Number(raw['instrument']),
    side: asSide(String(raw['side'])),
    kind: asKind(String(raw['kind'])),
    qty: qty(String(raw['qty'])),
    triggerPrice: price(String(raw['triggerPrice'])),
    limitPrice: limit === null || limit === undefined ? null : price(String(limit)),
  }
}

function parseQuotes(raw: unknown, at: number): FeedQuote[] {
  if (!Array.isArray(raw)) return []
  const out: FeedQuote[] = []
  for (const entry of raw as readonly Record<string, unknown>[]) {
    try {
      const source = entry['source']
      const rate = entry['rate']
      out.push({
        instrumentId: Number(entry['instrument']),
        bid: price(String(entry['bid'])),
        ask: price(String(entry['ask'])),
        at,
        ...(typeof source === 'string' && source !== '' ? { source } : {}),
        // A gateway that sends no rate is one that only lists instruments in
        // the account's currency, so parity is right there — but only there.
        ...(rate === undefined || rate === null ? {} : { rate: price(String(rate)) }),
      })
    } catch {
      // One malformed quote must not discard the batch.
    }
  }
  return out
}

/**
 * Handles one gateway frame.
 *
 * Pure and exported so `gateway.test.ts` can hold it to real payloads without
 * a socket. Returns the state patch and any event the terminal should show.
 */
export function handleFrame(
  text: string,
  at: number,
  previousQuotes: ReadonlyMap<number, FeedQuote>,
): { patch: Partial<GatewayState>; event?: GatewayEvent } {
  let raw: unknown
  try {
    raw = JSON.parse(text)
  } catch {
    return { patch: {} }
  }
  if (typeof raw !== 'object' || raw === null) return { patch: {} }
  const frame = raw as Record<string, unknown>

  switch (frame['type']) {
    case 'authRequired':
      // Everything else waits for this: the terminal shows a login form
      // rather than an empty platform.
      return { patch: { auth: 'required', session: null, status: 'live' } }

    case 'welcome': {
      const accounts = Array.isArray(frame['accounts'])
        ? (frame['accounts'] as Record<string, unknown>[]).map(parseSummary)
        : []
      const role = asRole(String(frame['role'] ?? 'client'))
      return {
        patch: {
          auth: 'signedIn',
          authError: null,
          session: {
            token: String(frame['token'] ?? ''),
            user: Number(frame['user']),
            login: String(frame['login']),
            role,
            accounts,
            account: Number(frame['account']),
            secondFactor: frame['secondFactor'] === true,
            secondFactorWanted: frame['secondFactorWanted'] === true,
          },
        },
      }
    }

    case 'verification':
      return {
        patch: {
          kyc: asKyc(String(frame['kyc'] ?? 'none')),
          mayGoLive: frame['mayGoLive'] === true,
        },
      }

    case 'authFailed': {
      const retry = frame['retryInSeconds']
      const reason = String(frame['reason'])
      return {
        patch: {
          auth: 'required',
          session: null,
          // The password was right and a code is wanted. Held apart from
          // the message so the sign-in screen can grow a field rather than
          // asking everybody for a code they have not got.
          codeWanted: reason === 'codeRequired' || reason === 'codeRefused',
          authError: authMessage(
            reason,
            retry === null || retry === undefined ? null : Number(retry),
          ),
        },
      }
    }

    case 'unauthorized':
      return {
        patch: {},
        event: { kind: 'unauthorized', detail: String(frame['detail'] ?? '') },
      }

    case 'paymentMethods': {
      const methods = Array.isArray(frame['methods'])
        ? (frame['methods'] as Record<string, unknown>[]).map(parseMethod)
        : []
      const mine = Array.isArray(frame['mine'])
        ? (frame['mine'] as Record<string, unknown>[]).map(parseRequest)
        : []
      return { patch: { methods, myDeposits: mine } }
    }

    case 'depositRequests': {
      const requests = Array.isArray(frame['requests'])
        ? (frame['requests'] as Record<string, unknown>[]).map(parseRequest)
        : []
      return { patch: { deposits: requests } }
    }

    case 'secondFactorSecret':
      return {
        patch: {
          secondFactorSecret: {
            secret: String(frame['secret'] ?? ''),
            grouped: String(frame['grouped'] ?? ''),
            uri: String(frame['uri'] ?? ''),
          },
        },
      }

    case 'payoutMethods': {
      const payouts = Array.isArray(frame['payouts'])
        ? (frame['payouts'] as Record<string, unknown>[]).map((p) => ({
            id: Number(p['id']),
            rail: asRail(String(p['rail'] ?? 'bank')),
            label: String(p['label'] ?? ''),
            address: String(p['address'] ?? ''),
            note: String(p['note'] ?? ''),
            preferred: p['preferred'] === true,
          }))
        : []
      return { patch: { payouts } }
    }

    case 'symbolSettings': {
      const symbols = Array.isArray(frame['symbols'])
        ? (frame['symbols'] as Record<string, unknown>[]).map((v) => ({
            instrument: Number(v['instrument']),
            symbol: String(v['symbol'] ?? ''),
            quote: String(v['quote'] ?? ''),
            displayDigits: Number(v['displayDigits'] ?? 2),
            tickSize: String(v['tickSize'] ?? '0'),
            contractSize: String(v['contractSize'] ?? '1'),
            minQty: String(v['minQty'] ?? '0'),
            maxQty: String(v['maxQty'] ?? '0'),
            qtyStep: String(v['qtyStep'] ?? '0'),
            // Absent means open: a gateway from before this existed had
            // no closed instruments, and reading silence as "closed"
            // would show every symbol shut.
            tradeable: v['tradeable'] !== false,
            overridden: v['overridden'] === true,
            note: typeof v['note'] === 'string' ? v['note'] : null,
          }))
        : []
      return { patch: { symbols } }
    }

    case 'instruments': {
      // The list changed under us — a symbol opened or closed, or its
      // sizes moved. The deal ticket reads these, so it has to be the
      // new ones or a trader is typing against rules that have gone.
      const instruments = Array.isArray(frame['instruments'])
        ? (frame['instruments'] as Record<string, unknown>[]).map(parseInstrument)
        : []
      return { patch: { instruments } }
    }

    case 'markups': {
      const pricing = Array.isArray(frame['markups'])
        ? (frame['markups'] as Record<string, unknown>[]).map((m) => ({
            instrument: Number(m['instrument']),
            symbol: String(m['symbol'] ?? ''),
            markup: String(m['markup'] ?? '0'),
            commission: String(m['commission'] ?? '0'),
            spread: m['spread'] == null ? null : String(m['spread']),
          }))
        : []
      return { patch: { pricing } }
    }

    case 'adminBook': {
      const accounts = Array.isArray(frame['accounts'])
        ? (frame['accounts'] as Record<string, unknown>[]).map(parseAdminAccount)
        : []
      const users = Array.isArray(frame['users'])
        ? (frame['users'] as Record<string, unknown>[]).map((u) => ({
            id: Number(u['id']),
            login: String(u['login']),
            role: asRole(String(u['role'] ?? 'client')),
            kyc: asKyc(String(u['kyc'] ?? 'none')),
            manager: u['manager'] === null || u['manager'] === undefined
              ? null
              : Number(u['manager']),
            managerLogin:
              typeof u['managerLogin'] === 'string' ? u['managerLogin'] : null,
            powers: Array.isArray(u['powers'])
              ? (u['powers'] as Record<string, unknown>[]).map(parsePower)
              : [],
          }))
        : []
      return { patch: { book: accounts, users } }
    }

    case 'candles': {
      const bars = Array.isArray(frame['bars'])
        ? (frame['bars'] as Record<string, unknown>[]).flatMap((b) => {
            try {
              return [
                {
                  open: price(String(b['open'])),
                  high: price(String(b['high'])),
                  low: price(String(b['low'])),
                  close: price(String(b['close'])),
                  // The gateway sends epoch *seconds*; the chart labels in
                  // the viewer's own time, as the exchange bars do.
                  label: hhmm(Number(b['start']) * 1000),
                },
              ]
            } catch {
              // One malformed bar must not blank the chart.
              return []
            }
          })
        : []
      return {
        patch: {
          candles: {
            instrumentId: Number(frame['instrument']),
            interval: String(frame['interval']),
            bars,
          },
        },
      }
    }

    case 'statement': {
      const entries = Array.isArray(frame['entries'])
        ? (frame['entries'] as Record<string, unknown>[]).flatMap((l) => {
            try {
              const ticket = l['ticket']
              return [
                {
                  seq: Number(l['seq']),
                  at: Number(l['at']),
                  kind: String(l['kind']) as StatementLine['kind'],
                  ticket: ticket === null || ticket === undefined ? null : Number(ticket),
                  amount: money(String(l['amount'])),
                  balance: money(String(l['balance'])),
                },
              ]
            } catch {
              return []
            }
          })
        : []
      return { patch: { statement: entries } }
    }

    case 'adminActivity': {
      const entries = Array.isArray(frame['entries'])
        ? (frame['entries'] as Record<string, unknown>[]).map((a) => ({
            seq: Number(a['seq']),
            at: Number(a['at']),
            user: Number(a['user']),
            login: typeof a['login'] === 'string' ? a['login'] : null,
            what: asActivity(String(a['what'])),
            address: String(a['address'] ?? ''),
          }))
        : []
      return { patch: { activity: entries } }
    }

    case 'adminBooks': {
      const raw = (frame['books'] ?? {}) as Record<string, unknown>
      const lines = Array.isArray(raw['lines'])
        ? (raw['lines'] as Record<string, unknown>[]).map((l) => ({
            owner: Number(l['owner']),
            house: l['house'] === true,
            book: String(l['book']),
            currency: String(l['currency']),
            amount: money(String(l['amount'])),
          }))
        : []
      const trial = Array.isArray(raw['trial'])
        ? (raw['trial'] as Record<string, unknown>[]).map((t) => ({
            currency: String(t['currency']),
            residual: money(String(t['residual'])),
          }))
        : []
      return {
        patch: {
          books: { lines, trial, transactions: Number(raw['transactions'] ?? 0) },
        },
      }
    }

    case 'adminAudit': {
      const entries = Array.isArray(frame['entries'])
        ? (frame['entries'] as Record<string, unknown>[]).map(parseAuditLine)
        : []
      return { patch: { audit: entries } }
    }

    case 'adminResult':
      return {
        patch: {
          adminResult: {
            done: frame['done'] === true,
            detail: String(frame['detail'] ?? ''),
          },
        },
      }

    case 'snapshot': {
      const quotes = new Map<number, FeedQuote>()
      for (const q of parseQuotes(frame['quotes'], at)) quotes.set(q.instrumentId, q)
      const instruments = Array.isArray(frame['instruments'])
        ? (frame['instruments'] as Record<string, unknown>[]).map(parseInstrument)
        : []
      const positions = Array.isArray(frame['positions'])
        ? (frame['positions'] as Record<string, unknown>[]).map(parsePosition)
        : []
      const pending = Array.isArray(frame['pending'])
        ? (frame['pending'] as Record<string, unknown>[]).map(parsePending)
        : []
      return {
        patch: {
          status: 'live',
          instruments,
          quotes,
          positions,
          pending,
          account: parseAccount((frame['account'] ?? {}) as Record<string, unknown>),
          seq: Number(frame['seq'] ?? 0),
        },
      }
    }

    case 'quotes': {
      const next = new Map(previousQuotes)
      for (const q of parseQuotes(frame['quotes'], at)) next.set(q.instrumentId, q)
      return { patch: { quotes: next } }
    }

    case 'portfolio': {
      const positions = Array.isArray(frame['positions'])
        ? (frame['positions'] as Record<string, unknown>[]).map(parsePosition)
        : []
      const pending = Array.isArray(frame['pending'])
        ? (frame['pending'] as Record<string, unknown>[]).map(parsePending)
        : []
      return {
        patch: {
          account: parseAccount((frame['account'] ?? {}) as Record<string, unknown>),
          positions,
          pending,
          seq: Number(frame['seq'] ?? 0),
        },
      }
    }

    case 'filled':
      return {
        patch: {},
        event: {
          kind: 'filled',
          ticket: Number(frame['ticket']),
          instrumentId: Number(frame['instrument']),
          side: asSide(String(frame['side'])),
          qty: qty(String(frame['qty'])),
          price: price(String(frame['price'])),
        },
      }

    case 'rejected':
      return {
        patch: {},
        event: { kind: 'rejected', reason: String(frame['reason']) as RejectReason },
      }

    case 'marginCall':
      return { patch: {}, event: { kind: 'marginCall', levelBp: Number(frame['levelBp']) } }

    case 'protected':
      return {
        patch: {},
        event: {
          kind: 'protected',
          ticket: Number(frame['ticket']),
          instrumentId: Number(frame['instrument']),
          protection: frame['kind'] === 'takeProfit' ? 'takeProfit' : 'stopLoss',
          qty: qty(String(frame['qty'])),
          price: price(String(frame['price'])),
        },
      }

    case 'amended':
      return {
        patch: {},
        event: {
          kind: 'amended',
          ticket: Number(frame['ticket']),
          amendment: frame['kind'] === 'correctPrice' ? 'correctPrice' : 'adjust',
          amount:
            frame['amount'] === null || frame['amount'] === undefined
              ? null
              : money(String(frame['amount'])),
          from: optionalPrice(frame['from']),
          to: optionalPrice(frame['to']),
          reason: asAmendReason(String(frame['reason'])),
        },
      }

    case 'protectionSet':
      return {
        patch: {},
        event: {
          kind: 'protectionSet',
          ticket: Number(frame['ticket']),
          stopLoss: optionalPrice(frame['stopLoss']),
          takeProfit: optionalPrice(frame['takeProfit']),
        },
      }

    case 'stoppedOut':
      return {
        patch: {},
        event: {
          kind: 'stoppedOut',
          instrumentId: Number(frame['instrument']),
          qty: qty(String(frame['qty'])),
          price: price(String(frame['price'])),
        },
      }

    case 'orderPlaced':
      return { patch: {}, event: { kind: 'orderPlaced', ticket: Number(frame['ticket']) } }

    case 'orderCancelled':
      return { patch: {}, event: { kind: 'orderCancelled', ticket: Number(frame['ticket']) } }

    case 'orderTriggered':
      return {
        patch: {},
        event: {
          kind: 'orderTriggered',
          ticket: Number(frame['ticket']),
          price: price(String(frame['price'])),
        },
      }

    case 'rollover':
      return {
        patch: {},
        event: {
          kind: 'rollover',
          day: Number(frame['day']),
          total: money(String(frame['total'] ?? '0')),
        },
      }

    default:
      return { patch: {} }
  }
}

/** Why a trade was put right. */
export type AmendReason =
  | 'badPrice'
  | 'dealingError'
  | 'feeReturned'
  | 'goodwill'
  | 'other'

/** What an administrator can do to an account. */
export type AdminActionKind =
  | 'deposit'
  | 'withdraw'
  | 'credit'
  | 'suspend'
  | 'reinstate'
  /** Let the account hold both sides of an instrument at once. */
  | 'hedging'
  /** Net opposing orders off again. */
  | 'netting'
  /** Make the account live: the money in it becomes the client's. */
  | 'live'
  /** Put it back to demo. */
  | 'demo'
  /** Give a demo account another thirty days. */
  | 'renew'

/** An order the terminal wants placed. */
export interface OrderRequest {
  readonly instrumentId: number
  readonly side: Side
  readonly kind: 'market' | 'limit' | 'stop' | 'stopLimit'
  readonly qty: Qty
  readonly price?: Price
  /** Where a stop-limit rests once its stop is reached. */
  readonly limitPrice?: Price
}

const parseMethod = (m: Record<string, unknown>): PaymentMethod => ({
  id: Number(m['id']),
  rail: asRail(String(m['rail'] ?? 'bank')),
  label: String(m['label'] ?? ''),
  address: String(m['address'] ?? ''),
  note: String(m['note'] ?? ''),
  enabled: m['enabled'] !== false,
})

const parseRequest = (r: Record<string, unknown>): DepositRequest => ({
  id: Number(r['id']),
  kind: asMoneyKind(String(r['kind'] ?? 'deposit')),
  pot: String(r['pot'] ?? 'balance'),
  account: Number(r['account']),
  user: Number(r['user']),
  login: typeof r['login'] === 'string' ? r['login'] : null,
  method: Number(r['method']),
  methodLabel: String(r['methodLabel'] ?? ''),
  rail: asRail(String(r['rail'] ?? 'bank')),
  amount: String(r['amount'] ?? ''),
  reference: String(r['reference'] ?? ''),
  at: Number(r['at'] ?? 0),
  stage:
    r['stage'] === 'credited' ? 'credited' : r['stage'] === 'refused' ? 'refused' : 'pending',
  settledBy: r['settledBy'] === null || r['settledBy'] === undefined ? null : Number(r['settledBy']),
  detail: String(r['detail'] ?? ''),
})

/** A live connection to the gateway. */
export class GatewayClient {
  private socket: WebSocket | null = null
  private timer: number | undefined
  private attempt = 0
  private closed = false
  private quotes: ReadonlyMap<number, FeedQuote> = new Map()

  constructor(
    private readonly url: string,
    private readonly handlers: GatewayHandlers,
  ) {}

  /**
   * Signs in with a password, and a code when the login has one.
   *
   * The password goes again with the code, because the gateway keeps
   * nothing between the two attempts: a half-authenticated session waiting
   * on a server is a session somebody else can finish.
   */
  login(login: string, password: string, code = ''): void {
    this.send({ type: 'login', login, password, code })
  }

  /** Signs in with a token from an earlier session on this gateway. */
  resume(token: string): void {
    this.send({ type: 'resume', token })
  }

  /** Acts on another account from now on. */
  selectAccount(account: number): void {
    this.send({ type: 'selectAccount', account })
  }

  /** Administrator: ask for the book and the people in it. */
  adminOverview(): void {
    this.send({ type: 'adminOverview' })
  }

  /** Asks for bars. The answer arrives as a `candles` frame. */
  requestCandles(instrumentId: number, interval: string, limit: number): void {
    this.send({ type: 'candles', instrument: instrumentId, interval, limit })
  }

  /** Asks for this account's statement. */
  requestStatement(limit = 100): void {
    this.send({ type: 'statement', limit })
  }

  /** Opens another account for the signed-in client. */
  openAccount(live: boolean): void {
    this.send({ type: 'openAccount', live })
  }

  /** Asks the desk to verify the signed-in client's identity. */
  requestVerification(): void {
    this.send({ type: 'requestVerification' })
  }

  /** Administrator: decide on a client's identity checks. */
  adminSetKyc(user: number, kyc: Kyc): void {
    this.send({ type: 'adminSetKyc', user, kyc })
  }

  /** Puts a client in a manager's book. Null takes them out of one. */
  adminAssignManager(user: number, manager: number | null): void {
    this.send({ type: 'adminAssignManager', user, manager })
  }

  adminSetRole(user: number, role: Role): void {
    this.send({ type: 'adminSetRole', user, role })
  }

  /** Asks how money may be paid in. */
  paymentMethods(): void {
    this.send({ type: 'paymentMethods' })
  }

  /** Tells the desk money has been sent. A claim, not a payment. */
  requestDeposit(method: number, amount: string, reference: string): void {
    this.send({ type: 'depositRequested', method, amount, reference })
  }

  adminPutPaymentMethod(m: {
    id: number | null
    rail: Rail
    label: string
    address: string
    note: string
    enabled: boolean
  }): void {
    this.send({ type: 'adminPutPaymentMethod', ...m })
  }

  adminRemovePaymentMethod(id: number): void {
    this.send({ type: 'adminRemovePaymentMethod', id })
  }

  adminDepositRequests(): void {
    this.send({ type: 'adminDepositRequests' })
  }

  /** Confirms the money arrived, or says it did not. */
  adminSettleDeposit(id: number, credit: boolean, detail: string): void {
    this.send({ type: 'adminSettleDeposit', id, credit, detail })
  }

  /** Approves or refuses anything in the queue, whatever kind. */
  adminSettle(id: number, approve: boolean, detail: string): void {
    this.send({ type: 'adminSettle', id, approve, detail })
  }

  /** The desk moving money without the client asking: bonus, insurance. */
  adminRaise(kind: MoneyKind, account: number, amount: string, note: string): void {
    this.send({ type: 'adminRaise', kind, account, amount, note })
  }

  /**
   * Moves money from the selected account to another.
   *
   * Not an administrator's call, though the gateway answers it with an
   * `adminResult`: a client moving their own money between their own
   * accounts is the main case, and the gateway checks whose accounts
   * these are rather than who is asking.
   */
  transfer(to: number, amount: string): void {
    this.send({ type: 'transfer', to, amount })
  }

  /** Asks to be paid out, to one of this client's saved places. */
  requestWithdrawal(payout: number, amount: string, note: string): void {
    this.send({ type: 'withdrawalRequested', payout, amount, note })
  }

  payoutMethods(): void {
    this.send({ type: 'payoutMethods' })
  }

  savePayoutMethod(m: {
    id: number | null
    rail: Rail
    label: string
    address: string
    note: string
    preferred: boolean
  }): void {
    this.send({ type: 'savePayoutMethod', ...m })
  }

  forgetPayoutMethod(id: number): void {
    this.send({ type: 'forgetPayoutMethod', id })
  }

  /** Starts setting up an authenticator. The secret comes back once. */
  beginSecondFactor(): void {
    this.send({ type: 'beginSecondFactor' })
  }

  /** Finishes setting one up by proving a code from it works. */
  confirmSecondFactor(code: string): void {
    this.send({ type: 'confirmSecondFactor', code })
  }

  /** Turns one off, which needs a current code from it. */
  stopSecondFactor(code: string): void {
    this.send({ type: 'stopSecondFactor', code })
  }

  /**
   * Administrator: give a manager one power, or take it away.
   *
   * One at a time rather than a whole map, so the audit of who changed
   * what reads as a list of decisions rather than a diff.
   */
  adminSetPower(user: number, power: string, allowed: boolean): void {
    this.send({ type: 'adminSetPower', user, power, allowed })
  }

  /** Every instrument, and what the desk has changed about it. */
  symbolSettings(): void {
    this.send({ type: 'symbolSettings' })
  }

  /** Opens an instrument for trading, or closes it. */
  adminSetSymbolOpen(instrument: number, open: boolean): void {
    this.send({ type: 'adminSetSymbolOpen', instrument, open })
  }

  /**
   * Sets the sizes an instrument accepts.
   *
   * All three together, because they only make sense together. All three
   * empty puts the catalogue's own back.
   */
  adminSetSymbolSizes(
    instrument: number,
    minQty: string,
    maxQty: string,
    qtyStep: string,
  ): void {
    this.send({ type: 'adminSetSymbolSizes', instrument, minQty, maxQty, qtyStep })
  }

  /** What the house charges, per instrument. */
  markups(): void {
    this.send({ type: 'markups' })
  }

  /**
   * Prices several instruments the same way at once.
   *
   * All or nothing at the gateway. An empty field means "leave it", so
   * setting a spread across a list does not restate the commission.
   */
  adminBulkPricing(instruments: readonly number[], markup: string, commission: string): void {
    this.send({ type: 'adminBulkPricing', instruments, markup, commission })
  }

  adminSetMarkup(instrument: number, markup: string): void {
    this.send({ type: 'adminSetMarkup', instrument, markup })
  }

  adminSetCommission(instrument: number, commission: string): void {
    this.send({ type: 'adminSetCommission', instrument, commission })
  }

  /** Opens another account for a client, as the desk does over the phone. */
  adminOpenAccount(user: number, live: boolean): void {
    this.send({ type: 'adminOpenAccount', user, live })
  }

  /** Administrator: ask who has been signing in. */
  adminActivity(limit = 100): void {
    this.send({ type: 'adminActivity', limit })
  }

  /** Administrator: ask for the books — one account, or everything. */
  adminBooks(account = 0): void {
    this.send({ type: 'adminBooks', account })
  }

  /** Administrator: ask for the audit trail. */
  adminAudit(limit = 100): void {
    this.send({ type: 'adminAudit', limit })
  }

  /**
   * Administrator: move money, set a bonus, or stop an account.
   *
   * The amount crosses as a decimal string, like every other money value —
   * see the note at the top of `gateway/src/protocol.rs`.
   */
  adminAction(account: number, kind: AdminActionKind, amount = ''): void {
    this.send({ type: 'adminAction', account, kind, amount })
  }

  /**
   * Administrator: put a trade right.
   *
   * `value` is a signed money amount for an adjustment, or the new entry
   * price for a correction. Both cross as decimal strings, like every other
   * money value.
   */
  adminAmend(
    account: number,
    ticket: number,
    kind: 'adjust' | 'correctPrice',
    reason: AmendReason,
    value: string,
  ): void {
    this.send({ type: 'adminAmend', account, ticket, kind, reason, value })
  }

  /** Administrator: create a client login. */
  adminCreateUser(login: string, password: string): void {
    this.send({ type: 'adminCreateUser', login, password })
  }

  /** Administrator: set someone's password. */
  adminSetPassword(user: number, password: string): void {
    this.send({ type: 'adminSetPassword', user, password })
  }

  /** Opens the connection and keeps it open. */
  connect(): void {
    if (this.closed) return
    this.handlers.onState({ status: this.attempt === 0 ? 'connecting' : 'reconnecting' })

    let ws: WebSocket
    try {
      ws = new WebSocket(this.url)
    } catch {
      this.scheduleRetry('could not open a socket')
      return
    }
    this.socket = ws

    ws.onopen = () => {
      this.attempt = 0
      // No `hello` until there is a session: the gateway answers nothing
      // before one, and asking is how a terminal used to start.
      this.handlers.onOpen?.()
    }

    ws.onmessage = (ev) => {
      const { patch, event } = handleFrame(String(ev.data), Date.now(), this.quotes)
      if (patch.quotes) this.quotes = patch.quotes
      if (Object.keys(patch).length > 0) this.handlers.onState(patch)
      if (event) this.handlers.onEvent(event)
    }

    ws.onclose = () => {
      if (this.closed) return
      this.scheduleRetry('gateway is not running')
    }

    ws.onerror = () => {
      // `onclose` always follows and carries the retry.
    }
  }

  private scheduleRetry(detail: string): void {
    this.attempt += 1
    this.handlers.onState({ status: 'reconnecting', detail })
    this.timer = window.setTimeout(() => this.connect(), backoffMs(this.attempt))
  }

  /** Sends an order. Silently does nothing when not connected. */
  place(order: OrderRequest): void {
    this.send({
      type: 'placeOrder',
      instrument: order.instrumentId,
      side: order.side,
      kind: order.kind,
      qty: order.qty.toString(),
      price: order.price ? order.price.toString() : '',
      limitPrice: order.limitPrice ? order.limitPrice.toString() : '',
    })
  }

  /**
   * Closes one position, or part of one.
   *
   * `qty` is a decimal string, and empty closes all of it. A string
   * because every size on this wire is one: a lot size that went through
   * a float would come back a different lot size.
   */
  close(ticket: number, qty = ''): void {
    this.send({ type: 'closePosition', ticket, qty })
  }

  /** Withdraws a resting order. */
  cancelOrder(ticket: number): void {
    this.send({ type: 'cancelOrder', ticket })
  }

  /**
   * Attaches, moves or removes a position's stop loss and take profit.
   *
   * Both are sent every time: the message says what the protection *is*, so a
   * terminal cannot half-apply it by forgetting a field. An empty string is
   * "none".
   */
  setProtection(ticket: number, stopLoss: Price | null, takeProfit: Price | null): void {
    this.send({
      type: 'setProtection',
      ticket,
      stopLoss: stopLoss === null ? '' : stopLoss.toString(),
      takeProfit: takeProfit === null ? '' : takeProfit.toString(),
    })
  }

  private send(payload: unknown): void {
    if (this.socket?.readyState !== WebSocket.OPEN) return
    this.socket.send(JSON.stringify(payload))
  }

  /** Stops reconnecting and closes. */
  disconnect(): void {
    this.closed = true
    if (this.timer !== undefined) window.clearTimeout(this.timer)
    this.socket?.close()
  }
}

/** Chart bars are still fetched from the exchange; the gateway does not store history. */
export type { Candle }
