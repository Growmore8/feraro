/**
 * React binding for the gateway connection.
 *
 * Mirrors `useFeed`: frames land in a mutable buffer and the component
 * repaints on a fixed cadence. The gateway already conflates on its side, but
 * a portfolio update arrives with every batch and re-rendering the blotter on
 * each one would undo that work.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { CONFLATION_MS } from './useFeed'
import type {
  AdminActionKind,
  AmendReason,
  Kyc,
  GatewayEvent,
  GatewayState,
  MoneyKind,
  OrderRequest,
  Rail,
  Role,
} from './gateway'
import type { Price } from '../lib/money'
import { GATEWAY_URL, GatewayClient } from './gateway'

/** What the terminal gets back. */
export interface GatewayView extends GatewayState {
  /** True when the engine is answering and orders can be placed. */
  readonly connected: boolean
  /** The most recent events, newest first, capped. */
  readonly events: readonly GatewayEvent[]
  placeOrder(order: OrderRequest): void
  closePosition(ticket: number, qty?: string): void
  cancelOrder(ticket: number): void
  setProtection(ticket: number, stopLoss: Price | null, takeProfit: Price | null): void
  acknowledge(index: number): void
  signIn(login: string, password: string, code?: string): void
  selectAccount(account: number): void
  signOut(): void
  adminOverview(): void
  adminAudit(limit?: number): void
  adminBooks(account?: number): void
  adminActivity(limit?: number): void
  openAccount(live: boolean): void
  requestVerification(): void
  adminSetKyc(user: number, kyc: Kyc): void
  adminAssignManager(user: number, manager: number | null): void
  adminSetRole(user: number, role: Role): void
  adminOpenAccount(user: number, live: boolean): void
  paymentMethods(): void
  requestDeposit(method: number, amount: string, reference: string): void
  adminPutPaymentMethod(m: {
    id: number | null
    rail: Rail
    label: string
    address: string
    note: string
    enabled: boolean
  }): void
  adminRemovePaymentMethod(id: number): void
  adminDepositRequests(): void
  adminSettleDeposit(id: number, credit: boolean, detail: string): void
  adminSettle(id: number, approve: boolean, detail: string): void
  adminRaise(kind: MoneyKind, account: number, amount: string, note: string): void
  requestWithdrawal(payout: number, amount: string, note: string): void
  transfer(to: number, amount: string): void
  beginSecondFactor(): void
  confirmSecondFactor(code: string): void
  stopSecondFactor(code: string): void
  payoutMethods(): void
  savePayoutMethod(m: {
    id: number | null
    rail: Rail
    label: string
    address: string
    note: string
    preferred: boolean
  }): void
  forgetPayoutMethod(id: number): void
  adminSetPower(user: number, power: string, allowed: boolean): void
  symbolSettings(): void
  adminSetSymbolOpen(instrument: number, open: boolean): void
  adminSetSymbolSizes(
    instrument: number,
    minQty: string,
    maxQty: string,
    qtyStep: string,
  ): void
  markups(): void
  adminBulkPricing(
    instruments: readonly number[],
    markup: string,
    commission: string,
  ): void
  adminSetMarkup(instrument: number, markup: string): void
  adminSetCommission(instrument: number, commission: string): void
  requestCandles(instrumentId: number, interval: string, limit: number): void
  requestStatement(limit?: number): void
  adminAction(account: number, kind: AdminActionKind, amount?: string): void
  adminAmend(
    account: number,
    ticket: number,
    kind: 'adjust' | 'correctPrice',
    reason: AmendReason,
    value: string,
  ): void
  adminCreateUser(login: string, password: string): void
  adminSetPassword(user: number, password: string): void
}

/**
 * Where the session token is kept.
 *
 * `sessionStorage`, not `localStorage`: a trading session should not outlive
 * the tab, and a token left behind on a shared machine is somebody else's
 * account. It is read once when the socket opens, so a page refresh does not
 * ask for a password again.
 */
const TOKEN_KEY = 'feraro.token'

function readToken(): string | null {
  try {
    return window.sessionStorage.getItem(TOKEN_KEY)
  } catch {
    // Private windows and blocked site data both throw rather than return
    // null. A terminal that cannot remember a token still works.
    return null
  }
}

function writeToken(token: string | null): void {
  try {
    if (token === null) window.sessionStorage.removeItem(TOKEN_KEY)
    else window.sessionStorage.setItem(TOKEN_KEY, token)
  } catch {
    // Nothing to do: the next reload asks for a password.
  }
}

const EMPTY: GatewayState = {
  status: 'idle',
  instruments: [],
  quotes: new Map(),
  account: null,
  positions: [],
  pending: [],
  seq: 0,
  auth: 'unknown',
  session: null,
  authError: null,
  kyc: 'none',
  mayGoLive: false,
  book: [],
  users: [],
  audit: [],
  books: null,
  activity: [],
  candles: null,
  statement: [],
  methods: [],
  myDeposits: [],
  deposits: [],
  codeWanted: false,
  secondFactorSecret: null,
  payouts: [],
  pricing: [],
  symbols: [],
  adminResult: null,
}

/** How many recent events are kept for the toast stack. */
const EVENT_LIMIT = 6

export function useGateway(url: string = GATEWAY_URL): GatewayView {
  const [state, setState] = useState<GatewayState>(EMPTY)
  const [events, setEvents] = useState<readonly GatewayEvent[]>([])

  const pending = useRef<Partial<GatewayState>>({})
  const clientRef = useRef<GatewayClient | null>(null)

  useEffect(() => {
    pending.current = {}

    const client = new GatewayClient(url, {
      onState: (patch) => {
        pending.current = { ...pending.current, ...patch }
        // Keep the token for a refresh, and drop it the moment the gateway
        // stops accepting it — a token that is not working is not worth
        // trying again with.
        if (patch.session?.token) writeToken(patch.session.token)
        if (patch.auth === 'required' && patch.authError) writeToken(null)
      },
      onEvent: (event) => {
        setEvents((prev) => [event, ...prev].slice(0, EVENT_LIMIT))
      },
      onOpen: () => {
        const token = readToken()
        if (token) client.resume(token)
      },
    })
    clientRef.current = client
    client.connect()

    const flush = window.setInterval(() => {
      const patch = pending.current
      if (Object.keys(patch).length === 0) return
      pending.current = {}
      setState((prev) => ({ ...prev, ...patch }))
    }, CONFLATION_MS)

    return () => {
      window.clearInterval(flush)
      client.disconnect()
      clientRef.current = null
    }
  }, [url])

  const placeOrder = useCallback((order: OrderRequest) => {
    clientRef.current?.place(order)
  }, [])

  const closePosition = useCallback((ticket: number, qty = '') => {
    clientRef.current?.close(ticket, qty)
  }, [])

  const cancelOrder = useCallback((ticket: number) => {
    clientRef.current?.cancelOrder(ticket)
  }, [])

  const setProtection = useCallback(
    (ticket: number, stopLoss: Price | null, takeProfit: Price | null) => {
      clientRef.current?.setProtection(ticket, stopLoss, takeProfit)
    },
    [],
  )

  const acknowledge = useCallback((index: number) => {
    setEvents((prev) => prev.filter((_, i) => i !== index))
  }, [])

  const signIn = useCallback((login: string, password: string, code = '') => {
    clientRef.current?.login(login, password, code)
    // `codeWanted` is left alone: a login that has been asked for a code is
    // still being asked for one while the second attempt is in flight, and
    // clearing it would make the field vanish under the person typing.
    setState((prev) => ({ ...prev, auth: 'signingIn', authError: null }))
  }, [])

  const selectAccount = useCallback((account: number) => {
    clientRef.current?.selectAccount(account)
  }, [])

  const adminOverview = useCallback(() => {
    clientRef.current?.adminOverview()
  }, [])

  const adminAudit = useCallback((limit = 100) => {
    clientRef.current?.adminAudit(limit)
  }, [])

  const adminBooks = useCallback((account = 0) => {
    clientRef.current?.adminBooks(account)
  }, [])

  const requestCandles = useCallback(
    (instrumentId: number, interval: string, limit: number) => {
      clientRef.current?.requestCandles(instrumentId, interval, limit)
    },
    [],
  )

  const requestStatement = useCallback((limit = 100) => {
    clientRef.current?.requestStatement(limit)
  }, [])

  const openAccount = useCallback((live: boolean) => {
    clientRef.current?.openAccount(live)
  }, [])

  const requestVerification = useCallback(() => {
    clientRef.current?.requestVerification()
  }, [])

  const adminAssignManager = useCallback((user: number, manager: number | null) => {
    clientRef.current?.adminAssignManager(user, manager)
  }, [])

  const adminSetRole = useCallback((user: number, role: Role) => {
    clientRef.current?.adminSetRole(user, role)
  }, [])

  const adminOpenAccount = useCallback((user: number, live: boolean) => {
    clientRef.current?.adminOpenAccount(user, live)
  }, [])

  const paymentMethods = useCallback(() => {
    clientRef.current?.paymentMethods()
  }, [])

  const requestDeposit = useCallback(
    (method: number, amount: string, reference: string) => {
      clientRef.current?.requestDeposit(method, amount, reference)
    },
    [],
  )

  const adminPutPaymentMethod = useCallback(
    (m: {
      id: number | null
      rail: Rail
      label: string
      address: string
      note: string
      enabled: boolean
    }) => {
      clientRef.current?.adminPutPaymentMethod(m)
    },
    [],
  )

  const adminRemovePaymentMethod = useCallback((id: number) => {
    clientRef.current?.adminRemovePaymentMethod(id)
  }, [])

  const adminDepositRequests = useCallback(() => {
    clientRef.current?.adminDepositRequests()
  }, [])

  const adminSettleDeposit = useCallback(
    (id: number, credit: boolean, detail: string) => {
      clientRef.current?.adminSettleDeposit(id, credit, detail)
    },
    [],
  )

  const adminSettle = useCallback((id: number, approve: boolean, detail: string) => {
    clientRef.current?.adminSettle(id, approve, detail)
  }, [])

  const adminRaise = useCallback(
    (kind: MoneyKind, account: number, amount: string, note: string) => {
      clientRef.current?.adminRaise(kind, account, amount, note)
    },
    [],
  )

  const transfer = useCallback((to: number, amount: string) => {
    clientRef.current?.transfer(to, amount)
  }, [])

  const beginSecondFactor = useCallback(() => {
    clientRef.current?.beginSecondFactor()
  }, [])

  const confirmSecondFactor = useCallback((code: string) => {
    clientRef.current?.confirmSecondFactor(code)
  }, [])

  const stopSecondFactor = useCallback((code: string) => {
    clientRef.current?.stopSecondFactor(code)
  }, [])

  const requestWithdrawal = useCallback(
    (payout: number, amount: string, note: string) => {
      clientRef.current?.requestWithdrawal(payout, amount, note)
    },
    [],
  )

  const payoutMethods = useCallback(() => {
    clientRef.current?.payoutMethods()
  }, [])

  const savePayoutMethod = useCallback(
    (m: {
      id: number | null
      rail: Rail
      label: string
      address: string
      note: string
      preferred: boolean
    }) => {
      clientRef.current?.savePayoutMethod(m)
    },
    [],
  )

  const forgetPayoutMethod = useCallback((id: number) => {
    clientRef.current?.forgetPayoutMethod(id)
  }, [])

  const adminSetPower = useCallback(
    (user: number, power: string, allowed: boolean) => {
      clientRef.current?.adminSetPower(user, power, allowed)
    },
    [],
  )

  const symbolSettings = useCallback(() => {
    clientRef.current?.symbolSettings()
  }, [])

  const adminSetSymbolOpen = useCallback((instrument: number, open: boolean) => {
    clientRef.current?.adminSetSymbolOpen(instrument, open)
  }, [])

  const adminSetSymbolSizes = useCallback(
    (instrument: number, minQty: string, maxQty: string, qtyStep: string) => {
      clientRef.current?.adminSetSymbolSizes(instrument, minQty, maxQty, qtyStep)
    },
    [],
  )

  const markups = useCallback(() => {
    clientRef.current?.markups()
  }, [])

  const adminBulkPricing = useCallback(
    (instruments: readonly number[], markup: string, commission: string) => {
      clientRef.current?.adminBulkPricing(instruments, markup, commission)
    },
    [],
  )

  const adminSetMarkup = useCallback((instrument: number, markup: string) => {
    clientRef.current?.adminSetMarkup(instrument, markup)
  }, [])

  const adminSetCommission = useCallback((instrument: number, commission: string) => {
    clientRef.current?.adminSetCommission(instrument, commission)
  }, [])

  const adminSetKyc = useCallback((user: number, kyc: Kyc) => {
    clientRef.current?.adminSetKyc(user, kyc)
  }, [])

  const adminActivity = useCallback((limit = 100) => {
    clientRef.current?.adminActivity(limit)
  }, [])

  const adminAction = useCallback(
    (account: number, kind: AdminActionKind, amount = '') => {
      clientRef.current?.adminAction(account, kind, amount)
    },
    [],
  )

  const adminAmend = useCallback(
    (
      account: number,
      ticket: number,
      kind: 'adjust' | 'correctPrice',
      reason: AmendReason,
      value: string,
    ) => {
      clientRef.current?.adminAmend(account, ticket, kind, reason, value)
    },
    [],
  )

  const adminCreateUser = useCallback((login: string, password: string) => {
    clientRef.current?.adminCreateUser(login, password)
  }, [])

  const adminSetPassword = useCallback((user: number, password: string) => {
    clientRef.current?.adminSetPassword(user, password)
  }, [])

  const signOut = useCallback(() => {
    writeToken(null)
    // Reload rather than unpick the state by hand: a fresh socket with no
    // token is exactly a signed-out terminal, and there is one code path to
    // get it wrong instead of two.
    window.location.reload()
  }, [])

  return useMemo(
    () => ({
      ...state,
      connected: state.auth === 'signedIn' && state.account !== null,
      events,
      placeOrder,
      closePosition,
      cancelOrder,
      setProtection,
      acknowledge,
      signIn,
      selectAccount,
      signOut,
      adminOverview,
      adminAudit,
      adminBooks,
      adminActivity,
      openAccount,
      requestVerification,
      adminSetKyc,
      adminAssignManager,
      adminSetRole,
      adminOpenAccount,
      paymentMethods,
      requestDeposit,
      adminPutPaymentMethod,
      adminRemovePaymentMethod,
      adminDepositRequests,
      adminSettleDeposit,
      adminSettle,
      adminRaise,
      requestWithdrawal,
      transfer,
      beginSecondFactor,
      confirmSecondFactor,
      stopSecondFactor,
      payoutMethods,
      savePayoutMethod,
      forgetPayoutMethod,
      adminSetPower,
      symbolSettings,
      adminSetSymbolOpen,
      adminSetSymbolSizes,
      markups,
      adminBulkPricing,
      adminSetMarkup,
      adminSetCommission,
      requestCandles,
      requestStatement,
      adminAction,
      adminAmend,
      adminCreateUser,
      adminSetPassword,
    }),
    [
      state,
      events,
      placeOrder,
      closePosition,
      cancelOrder,
      setProtection,
      acknowledge,
      signIn,
      selectAccount,
      signOut,
      adminOverview,
      adminAudit,
      adminBooks,
      adminActivity,
      openAccount,
      requestVerification,
      adminSetKyc,
      adminAssignManager,
      adminSetRole,
      adminOpenAccount,
      paymentMethods,
      requestDeposit,
      adminPutPaymentMethod,
      adminRemovePaymentMethod,
      adminDepositRequests,
      adminSettleDeposit,
      adminSettle,
      adminRaise,
      requestWithdrawal,
      transfer,
      beginSecondFactor,
      confirmSecondFactor,
      stopSecondFactor,
      payoutMethods,
      savePayoutMethod,
      forgetPayoutMethod,
      adminSetPower,
      symbolSettings,
      adminSetSymbolOpen,
      adminSetSymbolSizes,
      markups,
      adminBulkPricing,
      adminSetMarkup,
      adminSetCommission,
      requestCandles,
      requestStatement,
      adminAction,
      adminAmend,
      adminCreateUser,
      adminSetPassword,
    ],
  )
}
