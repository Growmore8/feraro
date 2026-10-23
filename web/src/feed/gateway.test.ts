import { describe, expect, it } from 'vitest'
import { handleFrame } from './gateway'
import type { FeedQuote } from './types'
import {
  convertPer,
  notional,
  price,
  qty,
  showMoney,
  showPrice,
  showQtyAsLots,
} from '../lib/money'

/**
 * The gateway's wire format, pinned from the Rust `serde` output. If a DTO in
 * `gateway/src/protocol.rs` is renamed, one of these fails rather than a field
 * quietly reading as `NaN` in front of a trader.
 */

const NO_QUOTES: ReadonlyMap<number, FeedQuote> = new Map()

const SNAPSHOT = JSON.stringify({
  type: 'snapshot',
  instruments: [
    {
      id: 1,
      symbol: 'BTCUSD',
      base: 'BTC',
      quote: 'USD',
      displayDigits: 2,
      contractSize: '1.00000000',
      tickSize: '0.010000000',
      minQty: '0.00100000',
      maxQty: '10.00000000',
      qtyStep: '0.00100000',
      tradeable: true,
      note: null,
    },
    {
      id: 30,
      symbol: 'USDJPY',
      base: 'USD',
      quote: 'JPY',
      displayDigits: 3,
      contractSize: '100000.00000000',
      tickSize: '0.001000000',
      minQty: '1000.00000000',
      maxQty: '5000000.00000000',
      qtyStep: '1000.00000000',
      tradeable: false,
      note: 'View only — quoted in a currency other than the account’s; needs currency conversion',
    },
  ],
  account: {
    id: 1001,
    currency: 'USD',
    leverage: 100,
    balance: '25000.000000000',
    credit: '500.000000000',
    equity: '25476.500000000',
    usedMargin: '784.010000000',
    freeMargin: '24692.490000000',
    marginLevelBp: 32495,
  },
  positions: [
    {
      ticket: 100000,
      instrument: 1,
      side: 'buy',
      qty: '0.50000000',
      openPrice: '78400.000000000',
      current: '78353.000000000',
      profit: '-23.500000000',
      swap: '-10.958904110',
      stopLoss: '77000.000000000',
      takeProfit: null,
    },
  ],
  quotes: [
    {
      instrument: 1,
      bid: '78353.000000000',
      ask: '78353.010000000',
      source: 'binance',
      rate: '1.000000000',
    },
    {
      instrument: 30,
      bid: '147.500000000',
      ask: '147.508000000',
      source: 'twelvedata',
      rate: '147.504000000',
    },
  ],
  seq: 4182,
})

describe('snapshot', () => {
  const { patch } = handleFrame(SNAPSHOT, 1_700_000_000_000, NO_QUOTES)

  it('goes live and carries the journal position', () => {
    expect(patch.status).toBe('live')
    expect(patch.seq).toBe(4182)
  })

  it('reads instrument limits into fixed-point', () => {
    const btc = patch.instruments![0]!
    expect(btc.symbol).toBe('BTCUSD')
    expect(btc.displayDigits).toBe(2)
    expect(showPrice(btc.tickSize, 2)).toBe('0.01')
    expect(btc.minQty).toBe(qty('0.001'))
    expect(btc.maxQty).toBe(qty('10'))
  })

  it('takes every account figure from the engine', () => {
    const a = patch.account!
    expect(showMoney(a.balance)).toBe('25,000.00')
    expect(showMoney(a.credit)).toBe('500.00')
    expect(showMoney(a.equity)).toBe('25,476.50')
    expect(showMoney(a.usedMargin)).toBe('784.01')
    expect(showMoney(a.freeMargin)).toBe('24,692.49')
    expect(a.marginLevelBp).toBe(32_495)
    // Netting unless the gateway says otherwise: an account that holds both
    // sides of a symbol is the exception, and the ticket's wording depends on
    // getting this the right way round.
    expect(a.hedging).toBe(false)
  })

  it('reads a hedging account as one', () => {
    const hedged = JSON.parse(SNAPSHOT)
    hedged.account.hedging = true
    const { patch } = handleFrame(JSON.stringify(hedged), 0, NO_QUOTES)
    expect(patch.account!.hedging).toBe(true)
  })

  it('reads positions with their engine-computed profit', () => {
    const p = patch.positions![0]!
    expect(p.ticket).toBe(100_000)
    expect(p.side).toBe('buy')
    expect(showQtyAsLots(p.qty, qty('1'))).toBe('0.50')
    expect(showPrice(p.openPrice, 2)).toBe('78400.00')
    expect(showPrice(p.current!, 2)).toBe('78353.00')
    expect(showMoney(p.profit)).toBe('-23.50')
    expect(showMoney(p.swap)).toBe('-10.96')
  })

  it('reads the levels a position is protected at, and none as none', () => {
    const p = patch.positions![0]!
    expect(showPrice(p.stopLoss!, 2)).toBe('77000.00')
    // Absent and zero both mean "no level", and the blotter must not show a
    // stop at zero — which would read as a stop one tick above nothing.
    expect(p.takeProfit).toBeNull()

    const zeroed = JSON.parse(SNAPSHOT)
    zeroed.positions[0].stopLoss = '0.000000000'
    const { patch: other } = handleFrame(JSON.stringify(zeroed), 0, NO_QUOTES)
    expect(other.positions![0]!.stopLoss).toBeNull()
  })

  it('treats a missing swap as zero rather than failing the whole snapshot', () => {
    const legacy = JSON.parse(SNAPSHOT)
    delete legacy.positions[0].swap
    const { patch } = handleFrame(JSON.stringify(legacy), 0, NO_QUOTES)
    expect(patch.positions![0]!.swap).toBe(0n)
  })

  it('indexes quotes by instrument', () => {
    const q = patch.quotes!.get(1)!
    expect(showPrice(q.bid, 2)).toBe('78353.00')
    expect(showPrice(q.ask, 2)).toBe('78353.01')
    expect(q.at).toBe(1_700_000_000_000)
    expect(q.source).toBe('binance')
    expect(q.rate).toBe(price('1'))
  })

  it('carries the conversion rate for an instrument quoted elsewhere', () => {
    // USDJPY prices in yen. 147,508 yen of notional is 1,000 dollars, and the
    // terminal needs the rate to say so before an order exists.
    const jpy = patch.quotes!.get(30)!
    expect(jpy.rate).toBe(price('147.504'))
    const asDollars = convertPer(notional(jpy.ask, qty('1000')), jpy.rate!)
    expect(showMoney(asDollars)).toBe('1,000.03')
  })

  it('keeps a view-only instrument listed, with the reason', () => {
    const [btc, jpy] = patch.instruments!
    expect(btc!.tradeable).toBe(true)
    expect(btc!.note).toBeNull()
    expect(jpy!.tradeable).toBe(false)
    expect(jpy!.note).toContain('currency conversion')
    expect(showQtyAsLots(qty('1000'), jpy!.contractSize)).toBe('0.01')
  })

  it('treats an instrument from an older gateway as tradeable', () => {
    const legacy = JSON.parse(SNAPSHOT)
    delete legacy.instruments[0].tradeable
    delete legacy.instruments[0].note
    delete legacy.quotes[0].source
    const { patch } = handleFrame(JSON.stringify(legacy), 0, NO_QUOTES)
    expect(patch.instruments![0]!.tradeable).toBe(true)
    expect(patch.quotes!.get(1)!.source).toBeUndefined()
  })
})

describe('signing in', () => {
  it('asks for credentials before anything else', () => {
    const { patch } = handleFrame(JSON.stringify({ type: 'authRequired' }), 0, NO_QUOTES)
    expect(patch.auth).toBe('required')
    expect(patch.session).toBeNull()
  })

  it('reads a welcome into a session with its accounts', () => {
    const frame = JSON.stringify({
      type: 'welcome',
      token: 'a'.repeat(64),
      user: 2,
      login: 'demo',
      role: 'client',
      account: 1001,
      accounts: [
        {
          id: 1001,
          currency: 'USD',
          balance: '25000.000000000',
          equity: '25476.500000000',
          owner: 2,
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.auth).toBe('signedIn')
    expect(patch.session?.login).toBe('demo')
    expect(patch.session?.role).toBe('client')
    expect(patch.session?.account).toBe(1001)
    expect(showMoney(patch.session!.accounts[0]!.equity)).toBe('25,476.50')
  })

  it('treats an unknown role as a client rather than trusting the wire', () => {
    const frame = JSON.stringify({
      type: 'welcome',
      token: 't',
      user: 9,
      login: 'who',
      role: 'superuser',
      account: 1001,
      accounts: [],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.session?.role).toBe('client')
  })

  it('turns a refusal into copy that gives nothing away', () => {
    const { patch } = handleFrame(
      JSON.stringify({ type: 'authFailed', reason: 'refused', retryInSeconds: null }),
      0,
      NO_QUOTES,
    )
    expect(patch.auth).toBe('required')
    expect(patch.authError).toBe('That login and password do not match.')
    // Not "no such user", and not "wrong password": the form must not say
    // which logins exist.
    expect(patch.authError).not.toMatch(/user|login does not/i)
  })

  it('says how long a locked-out login has to wait', () => {
    const { patch } = handleFrame(
      JSON.stringify({ type: 'authFailed', reason: 'tooManyAttempts', retryInSeconds: 45 }),
      0,
      NO_QUOTES,
    )
    expect(patch.authError).toContain('45 seconds')
  })

  it('reports being refused an account as an event, not a rejection', () => {
    const { patch, event } = handleFrame(
      JSON.stringify({ type: 'unauthorized', detail: 'that account is not yours to trade' }),
      0,
      NO_QUOTES,
    )
    expect(Object.keys(patch)).toHaveLength(0)
    expect(event).toEqual({
      kind: 'unauthorized',
      detail: 'that account is not yours to trade',
    })
  })
})

describe('the account statement', () => {
  it('reads movements with the balance each one left', () => {
    const frame = JSON.stringify({
      type: 'statement',
      entries: [
        {
          seq: 420,
          at: 1_700_000_100_000,
          kind: 'trade',
          ticket: 100_002,
          amount: '9.999900000',
          balance: '25009.999900000',
        },
        {
          seq: 401,
          at: 1_700_000_000_000,
          kind: 'withdrawal',
          ticket: null,
          amount: '-50.000000000',
          balance: '25000.000000000',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.statement).toHaveLength(2)
    expect(patch.statement![0]!.kind).toBe('trade')
    expect(showMoney(patch.statement![0]!.balance)).toBe('25,010.00')
    // A withdrawal is negative, so the page can be read down without
    // guessing which way the money went.
    expect(patch.statement![1]!.amount < 0n).toBe(true)
    expect(patch.statement![1]!.ticket).toBeNull()
  })

  it('drops a malformed line rather than the whole statement', () => {
    const { patch } = handleFrame(
      JSON.stringify({
        type: 'statement',
        entries: [
          { seq: 1, at: 0, kind: 'deposit', ticket: null, amount: 'oops', balance: '1' },
          { seq: 2, at: 0, kind: 'deposit', ticket: null, amount: '5', balance: '6' },
        ],
      }),
      0,
      NO_QUOTES,
    )
    expect(patch.statement).toHaveLength(1)
  })
})

describe('bars from the gateway', () => {
  it('reads bars into the shape the chart draws', () => {
    const frame = JSON.stringify({
      type: 'candles',
      instrument: 20,
      interval: '5m',
      bars: [
        {
          start: 1_700_000_000,
          open: '1.153800000',
          high: '1.154200000',
          low: '1.153500000',
          close: '1.154000000',
        },
        {
          start: 1_700_000_300,
          open: '1.154000000',
          high: '1.154500000',
          low: '1.153900000',
          close: '1.154400000',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.candles!.instrumentId).toBe(20)
    expect(patch.candles!.interval).toBe('5m')
    expect(patch.candles!.bars).toHaveLength(2)
    expect(showPrice(patch.candles!.bars[0]!.high, 5)).toBe('1.15420')
    // Labelled in the viewer's own time, like the exchange bars are.
    expect(patch.candles!.bars[0]!.label).toMatch(/^\d{2}:\d{2}$/)
  })

  it('drops a malformed bar rather than blanking the chart', () => {
    const frame = JSON.stringify({
      type: 'candles',
      instrument: 20,
      interval: '1m',
      bars: [
        { start: 1, open: 'oops', high: '1', low: '1', close: '1' },
        { start: 2, open: '1.1', high: '1.2', low: '1.0', close: '1.15' },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.candles!.bars).toHaveLength(1)
  })

  it('reports an empty series rather than failing when there is no history', () => {
    const { patch } = handleFrame(
      JSON.stringify({ type: 'candles', instrument: 24, interval: '1h', bars: [] }),
      0,
      NO_QUOTES,
    )
    expect(patch.candles!.bars).toEqual([])
  })
})

describe('client activity', () => {
  it('reads sign-ins, refusals and sign-outs with where they came from', () => {
    const frame = JSON.stringify({
      type: 'adminActivity',
      entries: [
        {
          seq: 412,
          at: 1_700_000_000_000,
          user: 2,
          login: 'demo',
          what: 'signedIn',
          address: '127.0.0.1',
        },
        {
          seq: 410,
          at: 1_699_999_000_000,
          user: 0,
          login: null,
          what: 'refused',
          address: '203.0.113.7',
        },
        { seq: 409, at: 0, user: 2, login: 'demo', what: 'signedOut', address: '' },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.activity).toHaveLength(3)
    expect(patch.activity![0]!.what).toBe('signedIn')
    expect(patch.activity![0]!.login).toBe('demo')
    // A refused attempt names no user, because it proved nothing about who it
    // was — the screen says "unknown" rather than inventing user 0.
    expect(patch.activity![1]!.user).toBe(0)
    expect(patch.activity![1]!.login).toBeNull()
    expect(patch.activity![2]!.address).toBe('')
  })
})

describe('the books', () => {
  it('reads the pots and the trial balance', () => {
    const frame = JSON.stringify({
      type: 'adminBooks',
      books: {
        lines: [
          {
            owner: 1001,
            house: false,
            book: 'ClientCash',
            currency: 'USD',
            amount: '25010.000000000',
          },
          {
            owner: 1001,
            house: false,
            book: 'Bonus',
            currency: 'USD',
            amount: '500.000000000',
          },
          {
            owner: 0,
            house: true,
            book: 'RealisedPnl',
            currency: 'USD',
            amount: '-10.000000000',
          },
          {
            owner: 0,
            house: true,
            book: 'PspClearing',
            currency: 'USD',
            amount: '-25000.000000000',
          },
          {
            owner: 0,
            house: true,
            book: 'Bonus',
            currency: 'USD',
            amount: '-500.000000000',
          },
        ],
        trial: [{ currency: 'USD', residual: '0.000000000' }],
        transactions: 4,
      },
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.books!.transactions).toBe(4)
    expect(showMoney(patch.books!.lines[0]!.amount)).toBe('25,010.00')
    expect(patch.books!.lines[2]!.house).toBe(true)
    // Zero is the only acceptable residual, and the screen shows it as
    // "balanced" rather than as a number nobody should have to read.
    expect(patch.books!.trial[0]!.residual).toBe(0n)

    // The sum of every pot is the invariant: money is moved, never made.
    const total = patch.books!.lines.reduce((acc, l) => acc + l.amount, 0n)
    expect(total).toBe(0n)
  })
})


describe('what a desk may do', () => {
  it('reads a manager’s powers, and which cannot be given', () => {
    const frame = JSON.stringify({
      type: 'adminBook',
      accounts: [],
      users: [
        {
          id: 7,
          login: 'desk@feraro.local',
          role: 'manager',
          kyc: 'none',
          manager: null,
          managerLogin: null,
          powers: [
            { key: 'processDeposits', said: 'Approve deposits', held: true, possible: true, whyNot: null },
            { key: 'processWithdrawals', said: 'Approve withdrawals', held: false, possible: true, whyNot: null },
            {
              key: 'viewAudit',
              said: 'Read the audit trail',
              held: false,
              possible: false,
              whyNot: 'the audit trail covers every manager’s clients',
            },
          ],
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    const desk = patch.users![0]!
    expect(desk.powers.find((p) => p.key === 'processDeposits')!.held).toBe(true)
    expect(desk.powers.find((p) => p.key === 'processWithdrawals')!.held).toBe(false)
    // The impossible one stays on the list with its reason, so the screen
    // can grey it out rather than leaving somebody hunting for it.
    const audit = desk.powers.find((p) => p.key === 'viewAudit')!
    expect(audit.possible).toBe(false)
    expect(audit.whyNot).toMatch(/audit trail/)
  })

  it('gives an administrator and a client no switches at all', () => {
    // Neither is a row of switches that would do anything: an
    // administrator holds everything, a client holds none of it.
    const frame = JSON.stringify({
      type: 'adminBook',
      accounts: [],
      users: [
        { id: 1, login: 'boss@feraro.local', role: 'admin', kyc: 'none', manager: null, managerLogin: null },
        { id: 2, login: 'trader@example.com', role: 'client', kyc: 'approved', manager: 7, managerLogin: 'desk@feraro.local' },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.users![0]!.powers).toEqual([])
    expect(patch.users![1]!.powers).toEqual([])
  })

  it('reads a gateway that predates permissions as nothing forbidden', () => {
    // Silence must not grey out every switch on the screen.
    const frame = JSON.stringify({
      type: 'adminBook',
      accounts: [],
      users: [
        {
          id: 7,
          login: 'desk@feraro.local',
          role: 'manager',
          kyc: 'none',
          manager: null,
          managerLogin: null,
          powers: [{ key: 'processDeposits', said: 'Approve deposits', held: true }],
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.users![0]!.powers[0]!.possible).toBe(true)
    expect(patch.users![0]!.powers[0]!.whyNot).toBeNull()
  })
})

describe('two-factor', () => {
  it('reads whether the login has one, and whether it ought to', () => {
    const frame = JSON.stringify({
      type: 'welcome',
      token: 'abc',
      user: 1,
      login: 'desk@feraro.local',
      role: 'admin',
      accounts: [],
      account: 1001,
      secondFactor: false,
      secondFactorWanted: true,
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.session!.secondFactor).toBe(false)
    // The terminal nags on this, every time, until it is dealt with.
    expect(patch.session!.secondFactorWanted).toBe(true)
  })

  it('reads an older welcome frame as a login with none, and no nag', () => {
    // A gateway from before two-factor existed. Reading the absent field as
    // "wanted" would put a warning on every desk screen that nothing could
    // clear, because that gateway has nothing to enrol with.
    const frame = JSON.stringify({
      type: 'welcome',
      token: 'abc',
      user: 1,
      login: 'desk@feraro.local',
      role: 'admin',
      accounts: [],
      account: 1001,
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.session!.secondFactor).toBe(false)
    expect(patch.session!.secondFactorWanted).toBe(false)
  })

  it('asks for a code only when the gateway says to', () => {
    // The password was right. A sign-in screen that asked everybody for a
    // code would ask the clients, who have none.
    const wanted = handleFrame(
      JSON.stringify({ type: 'authFailed', reason: 'codeRequired' }),
      0,
      NO_QUOTES,
    )
    expect(wanted.patch.codeWanted).toBe(true)
    expect(wanted.patch.authError).toMatch(/authenticator/i)

    // A code that was not accepted keeps the field, because the next one
    // will be typed into it.
    const refused = handleFrame(
      JSON.stringify({ type: 'authFailed', reason: 'codeRefused' }),
      0,
      NO_QUOTES,
    )
    expect(refused.patch.codeWanted).toBe(true)
    // Not "wrong code": it may have been right and already used, and
    // somebody who reads "wrong" retypes the same one.
    expect(refused.patch.authError).not.toMatch(/wrong/i)

    // An ordinary refusal does not. Which half was wrong is not the
    // trader's business and not an attacker's to learn.
    const plain = handleFrame(
      JSON.stringify({ type: 'authFailed', reason: 'refused' }),
      0,
      NO_QUOTES,
    )
    expect(plain.patch.codeWanted).toBe(false)
  })

  it('carries a fresh secret in three forms, because each is read differently', () => {
    const frame = JSON.stringify({
      type: 'secondFactorSecret',
      secret: 'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ',
      grouped: 'GEZD GNBV GY3T QOJQ GEZD GNBV GY3T QOJQ',
      uri: 'otpauth://totp/Feraro:desk%40feraro.local?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=Feraro&algorithm=SHA1&digits=6&period=30',
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    // Flat to paste, grouped to read off a screen, and a URI that opens an
    // app on a phone.
    expect(patch.secondFactorSecret!.secret).toHaveLength(32)
    expect(patch.secondFactorSecret!.grouped).toContain(' ')
    expect(patch.secondFactorSecret!.uri).toMatch(/^otpauth:\/\/totp\//)
  })
})

describe('the money queue', () => {
  it('reads every kind of movement, and which pot each touches', () => {
    const frame = JSON.stringify({
      type: 'depositRequests',
      requests: [
        {
          id: 7,
          kind: 'withdrawal',
          pot: 'balance',
          account: 1001,
          user: 2,
          login: 'trader@example.com',
          method: 3,
          methodLabel: 'HDFC savings',
          rail: 'bank',
          amount: '400.00',
          reference: '',
          at: 1_700_000_000_000,
          stage: 'pending',
          settledBy: null,
          detail: '',
        },
        {
          id: 8,
          kind: 'insuranceIn',
          pot: 'insurance',
          account: 1001,
          user: 2,
          login: 'trader@example.com',
          method: 0,
          methodLabel: '',
          rail: 'bank',
          amount: '1000.00',
          reference: '',
          at: 1_700_000_100_000,
          stage: 'credited',
          settledBy: 1,
          detail: 'checked at the desk',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.deposits!.map((r) => r.kind)).toEqual(['withdrawal', 'insuranceIn'])
    expect(patch.deposits!.map((r) => r.pot)).toEqual(['balance', 'insurance'])
    // The amount stays the string the client claimed. Formatting it would
    // dress a claim up as an agreed figure.
    expect(patch.deposits![0]!.amount).toBe('400.00')
  })

  it('calls an old frame without a kind a deposit, rather than reading NaN', () => {
    // A gateway from before the queue held more than deposits. The screen
    // must still show the row: the money is real either way.
    const frame = JSON.stringify({
      type: 'depositRequests',
      requests: [
        {
          id: 1,
          account: 1001,
          user: 2,
          login: null,
          method: 1,
          methodLabel: 'USDT TRC20',
          rail: 'crypto',
          amount: '100.00',
          reference: 'abc',
          at: 1_700_000_000_000,
          stage: 'pending',
          settledBy: null,
          detail: '',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.deposits![0]!.kind).toBe('deposit')
    expect(patch.deposits![0]!.pot).toBe('balance')
  })

  it('reads where a client wants to be paid, and which one is the usual', () => {
    const frame = JSON.stringify({
      type: 'payoutMethods',
      payouts: [
        {
          id: 1,
          rail: 'crypto',
          label: 'Ledger',
          address: 'TR7NHq...',
          note: 'TRC20',
          preferred: true,
        },
        {
          id: 2,
          rail: 'upi',
          label: 'GPay',
          address: 'someone@okaxis',
          note: '',
          preferred: false,
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.payouts!.find((p) => p.preferred)!.label).toBe('Ledger')
    // In full. A truncated address is one nobody can check.
    expect(patch.payouts![0]!.address).toBe('TR7NHq...')
    expect(patch.payouts![1]!.rail).toBe('upi')
  })
})

describe('a transfer on the statement', () => {
  it('reads as two legs at one moment, one out and one in', () => {
    // Both carry the same sequence number because they are one journal
    // record. A screen that sorted them apart would show a moment when the
    // money was in neither account, and somebody would go looking for it.
    const frame = JSON.stringify({
      type: 'statement',
      entries: [
        {
          seq: 256,
          at: 1_700_000_000_000,
          kind: 'transferOut',
          ticket: null,
          amount: '-2500.000000000',
          balance: '2500.000000000',
        },
        {
          seq: 256,
          at: 1_700_000_000_000,
          kind: 'transferIn',
          ticket: null,
          amount: '2500.000000000',
          balance: '2500.000000000',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    const [out, into] = patch.statement!
    expect(out!.kind).toBe('transferOut')
    expect(into!.kind).toBe('transferIn')
    expect(out!.seq).toBe(into!.seq)
    // The pair nets to nothing: a transfer moves money, it does not make or
    // destroy any, and a statement that summed them to anything else would
    // be reporting a platform that had invented some.
    expect(out!.amount + into!.amount).toBe(0n)
    expect(showMoney(into!.amount)).toBe('2,500.00')
  })
})

describe('pricing', () => {
  it('reads the markup, the commission and the spread they produce', () => {
    const frame = JSON.stringify({
      type: 'markups',
      markups: [
        {
          instrument: 1,
          symbol: 'BTCUSD',
          markup: '0.50000',
          commission: '2.00',
          spread: '1.70',
        },
        {
          instrument: 30,
          symbol: 'USDJPY',
          markup: '0',
          commission: '0',
          spread: null,
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    // Decimal strings, never numbers: a markup that went through a float
    // would come back a different markup.
    expect(patch.pricing![0]!.markup).toBe('0.50000')
    expect(patch.pricing![0]!.spread).toBe('1.70')
    // An unquoted instrument has no spread, and null is not '0': zero would
    // read as a market with no spread at all.
    expect(patch.pricing![1]!.spread).toBeNull()
  })
})

describe('insurance on an account', () => {
  it('is read apart from the bonus, because it is not the same money', () => {
    const frame = JSON.stringify({
      type: 'snapshot',
      instruments: [],
      account: {
        id: 1001,
        currency: 'USD',
        leverage: 100,
        balance: '5000.000000000',
        credit: '250.000000000',
        insurance: '1000.000000000',
        equity: '5250.000000000',
        usedMargin: '0.000000000',
        freeMargin: '4250.000000000',
        marginLevelBp: null,
        hedging: false,
        live: true,
        demoExpiresAt: null,
      },
      positions: [],
      pending: [],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(showMoney(patch.account!.insurance)).toBe('1,000.00')
    expect(showMoney(patch.account!.credit)).toBe('250.00')
    // Free margin is below the equity by the amount set aside: insurance
    // supports no margin, which is the whole difference from a bonus.
    expect(patch.account!.equity - patch.account!.freeMargin).toBe(
      patch.account!.insurance,
    )
  })

  it('reads an older account frame as nothing set aside', () => {
    const frame = JSON.stringify({
      type: 'snapshot',
      instruments: [],
      account: {
        id: 1001,
        currency: 'USD',
        leverage: 100,
        balance: '100.000000000',
        credit: '0.000000000',
        equity: '100.000000000',
        usedMargin: '0.000000000',
        freeMargin: '100.000000000',
        marginLevelBp: null,
        hedging: false,
        live: false,
        demoExpiresAt: null,
      },
      positions: [],
      pending: [],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.account!.insurance).toBe(0n)
  })
})

describe('protection', () => {
  it('reports a stop loss and a take profit as different things', () => {
    const stop = handleFrame(
      JSON.stringify({
        type: 'protected',
        ticket: 100_001,
        instrument: 1,
        kind: 'stopLoss',
        qty: '0.01000000',
        price: '78900.000000000',
      }),
      0,
      NO_QUOTES,
    )
    expect(stop.event).toEqual({
      kind: 'protected',
      ticket: 100_001,
      instrumentId: 1,
      protection: 'stopLoss',
      qty: qty('0.01'),
      price: price('78900'),
    })

    const target = handleFrame(
      JSON.stringify({
        type: 'protected',
        ticket: 100_002,
        instrument: 1,
        kind: 'takeProfit',
        qty: '0.01000000',
        price: '82000.000000000',
      }),
      0,
      NO_QUOTES,
    )
    expect(target.event).toMatchObject({ protection: 'takeProfit' })
  })

  it('confirms what the protection now is, including nothing', () => {
    const set = handleFrame(
      JSON.stringify({
        type: 'protectionSet',
        ticket: 100_001,
        stopLoss: '79000.000000000',
        takeProfit: null,
      }),
      0,
      NO_QUOTES,
    )
    expect(set.event).toMatchObject({
      kind: 'protectionSet',
      ticket: 100_001,
      takeProfit: null,
    })

    const cleared = handleFrame(
      JSON.stringify({ type: 'protectionSet', ticket: 100_001 }),
      0,
      NO_QUOTES,
    )
    expect(cleared.event).toMatchObject({ stopLoss: null, takeProfit: null })
  })
})

describe('the administrator’s book', () => {
  it('reads accounts with the engine’s figures and the session’s logins', () => {
    const frame = JSON.stringify({
      type: 'adminBook',
      accounts: [
        {
          id: 1001,
          owner: 2,
          ownerLogin: 'demo',
          currency: 'USD',
          leverage: 100,
          balance: '25000.000000000',
          credit: '500.000000000',
          equity: '25476.500000000',
          usedMargin: '784.010000000',
          marginLevelBp: 32495,
          positions: 2,
          pending: 1,
          tradeable: true,
        },
        {
          id: 1002,
          owner: null,
          ownerLogin: null,
          currency: 'USD',
          leverage: 100,
          balance: '100.000000000',
          credit: '0.000000000',
          equity: '100.000000000',
          usedMargin: '0.000000000',
          marginLevelBp: null,
          positions: 0,
          pending: 0,
          tradeable: false,
        },
      ],
      users: [
        { id: 1, login: 'admin', role: 'admin' },
        { id: 2, login: 'demo', role: 'client' },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    const [first, second] = patch.book!
    expect(showMoney(first!.equity)).toBe('25,476.50')
    expect(first!.ownerLogin).toBe('demo')
    expect(first!.positions).toBe(2)
    expect(first!.tradeable).toBe(true)
    // An account nobody has claimed is shown as such, not as somebody's.
    expect(second!.owner).toBeNull()
    expect(second!.marginLevelBp).toBeNull()
    expect(second!.tradeable).toBe(false)
    expect(patch.users!.map((u) => u.role)).toEqual(['admin', 'client'])
    // A people list written before managers existed says nobody manages
    // anybody, which is the reading that gives away no book.
    expect(patch.users!.every((u) => u.manager === null)).toBe(true)
  })

  it('reads a manager, and the book each client sits in', () => {
    const frame = JSON.stringify({
      type: 'adminBook',
      accounts: [],
      users: [
        { id: 1, login: 'admin@feraro.local', role: 'admin', kyc: 'none' },
        { id: 7, login: 'boss@feraro.local', role: 'manager', kyc: 'none' },
        {
          id: 9,
          login: 'trader@example.com',
          role: 'client',
          kyc: 'approved',
          manager: 7,
          managerLogin: 'boss@feraro.local',
        },
        { id: 10, login: 'loose@example.com', role: 'client', kyc: 'none', manager: null },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    const [, boss, assigned, loose] = patch.users!
    expect(boss!.role).toBe('manager')
    expect(assigned!.manager).toBe(7)
    expect(assigned!.managerLogin).toBe('boss@feraro.local')
    // Nobody's client is nobody's, not the first manager on the list.
    expect(loose!.manager).toBeNull()
    expect(loose!.managerLogin).toBeNull()
  })

  it('reads a manager signing in as a manager, not as a client', () => {
    const frame = JSON.stringify({
      type: 'welcome',
      token: 'a-token',
      user: 7,
      login: 'boss@feraro.local',
      role: 'manager',
      account: 1007,
      accounts: [
        {
          id: 1007,
          currency: 'USD',
          balance: '0.000000000',
          equity: '0.000000000',
          owner: 7,
          live: false,
          demoExpiresAt: null,
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.session!.role).toBe('manager')

    // A role the browser does not know is read as the least of them, never
    // as the most: an unknown word must not become an administrator.
    const odd = handleFrame(frame.replace('"manager"', '"superuser"'), 0, NO_QUOTES)
    expect(odd.patch.session!.role).toBe('client')
  })

  it('reads the audit trail, platform actions included', () => {
    const frame = JSON.stringify({
      type: 'adminAudit',
      entries: [
        {
          seq: 412,
          at: 1_700_000_000_000,
          by: 1,
          byLogin: 'admin',
          account: 1001,
          action: 'deposit',
          detail: '2500.000000000',
        },
        {
          seq: 3,
          at: 1_699_999_000_000,
          by: 0,
          byLogin: null,
          account: 1001,
          action: 'accountOpened',
          detail: '',
        },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.audit).toHaveLength(2)
    expect(patch.audit![0]!.byLogin).toBe('admin')
    // Zero is the platform itself, and the screen says so rather than
    // inventing a user named "0".
    expect(patch.audit![1]!.by).toBe(0)
    expect(patch.audit![1]!.byLogin).toBeNull()
  })

  it('keeps what came of the last action, refusals included', () => {
    const done = handleFrame(
      JSON.stringify({ type: 'adminResult', done: true, detail: 'Deposited 100 to 1001.' }),
      0,
      NO_QUOTES,
    )
    expect(done.patch.adminResult).toEqual({ done: true, detail: 'Deposited 100 to 1001.' })

    const refused = handleFrame(
      JSON.stringify({ type: 'adminResult', done: false, detail: 'Refused: no such account.' }),
      0,
      NO_QUOTES,
    )
    expect(refused.patch.adminResult!.done).toBe(false)
  })
})

describe('incremental frames', () => {
  it('merges a quote batch over what is already known', () => {
    const before = new Map([
      [1, { instrumentId: 1, bid: 1n as never, ask: 2n as never, at: 0 }],
      [2, { instrumentId: 2, bid: 3n as never, ask: 4n as never, at: 0 }],
    ])
    const frame = JSON.stringify({
      type: 'quotes',
      quotes: [{ instrument: 1, bid: '78400.000000000', ask: '78400.010000000' }],
    })
    const { patch } = handleFrame(frame, 5, before)
    expect(patch.quotes!.size).toBe(2)
    expect(showPrice(patch.quotes!.get(1)!.bid, 2)).toBe('78400.00')
    // Instrument 2 was not in the batch and must survive untouched.
    expect(patch.quotes!.get(2)!.at).toBe(0)
  })

  it('replaces the whole portfolio rather than patching it', () => {
    const frame = JSON.stringify({
      type: 'portfolio',
      account: {
        id: 1001,
        currency: 'USD',
        leverage: 100,
        balance: '100.000000000',
        credit: '0.000000000',
        equity: '100.000000000',
        usedMargin: '0.000000000',
        freeMargin: '100.000000000',
        marginLevelBp: null,
      },
      positions: [],
      seq: 9,
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.positions).toEqual([])
    expect(patch.account!.marginLevelBp).toBeNull()
  })
})

describe('events', () => {
  it('reads a fill', () => {
    const frame = JSON.stringify({
      type: 'filled',
      ticket: 100001,
      instrument: 1,
      side: 'sell',
      qty: '0.25000000',
      price: '78353.000000000',
    })
    const { event } = handleFrame(frame, 0, NO_QUOTES)
    expect(event).toEqual({
      kind: 'filled',
      ticket: 100001,
      instrumentId: 1,
      side: 'sell',
      qty: qty('0.25'),
      price: 78_353_000_000_000n,
    })
  })

  it('reads a rejection by the engine’s own variant name', () => {
    const { event } = handleFrame(
      JSON.stringify({ type: 'rejected', reason: 'insufficientMargin' }),
      0,
      NO_QUOTES,
    )
    expect(event).toEqual({ kind: 'rejected', reason: 'insufficientMargin' })
  })

  it('reads a margin call and a stop-out', () => {
    const call = handleFrame(JSON.stringify({ type: 'marginCall', levelBp: 8_400 }), 0, NO_QUOTES)
    expect(call.event).toEqual({ kind: 'marginCall', levelBp: 8_400 })

    const out = handleFrame(
      JSON.stringify({
        type: 'stoppedOut',
        instrument: 1,
        qty: '0.50000000',
        price: '70000.000000000',
      }),
      0,
      NO_QUOTES,
    )
    expect(out.event?.kind).toBe('stoppedOut')
  })
})

describe('rollover', () => {
  it('reads the rollover announcement', () => {
    const { event } = handleFrame(
      JSON.stringify({ type: 'rollover', day: 20710, total: '-21.917808219' }),
      0,
      NO_QUOTES,
    )
    expect(event?.kind).toBe('rollover')
    if (event?.kind === 'rollover') {
      expect(event.day).toBe(20710)
      expect(showMoney(event.total)).toBe('-21.92')
    }
  })
})

describe('robustness', () => {
  it('ignores anything it cannot read rather than throwing at the socket', () => {
    for (const bad of ['', 'not json', '[]', 'null', '{"type":"somethingNew"}']) {
      expect(() => handleFrame(bad, 0, NO_QUOTES)).not.toThrow()
      expect(handleFrame(bad, 0, NO_QUOTES).patch).toEqual({})
    }
  })

  it('drops one malformed quote and keeps the rest of the batch', () => {
    const frame = JSON.stringify({
      type: 'quotes',
      quotes: [
        { instrument: 1, bid: 'oops', ask: '1.0' },
        { instrument: 2, bid: '2.000000000', ask: '2.010000000' },
      ],
    })
    const { patch } = handleFrame(frame, 0, NO_QUOTES)
    expect(patch.quotes!.has(1)).toBe(false)
    expect(patch.quotes!.has(2)).toBe(true)
  })
})
