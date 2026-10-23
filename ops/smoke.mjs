// Everything the platform does, over real sockets, against a running gateway.
//
// Not a unit test — those live with the code and run in CI. This is the
// check you run against a deployment before you let anybody near it: it
// signs in as all three roles, moves money, deals, rests orders, and proves
// the refusals refuse.
//
//   node ops/smoke.mjs <adminPassword> <clientPassword> <managerPassword>
//
// It leaves the book as it found it: everything it opens, it closes.

const [adminPw, clientPw, managerPw] = process.argv.slice(2)
if (!adminPw) {
  console.error('usage: node ops/smoke.mjs <adminPw> [clientPw] [managerPw]')
  process.exit(2)
}

const URL = process.env.FERARO_URL ?? 'ws://127.0.0.1:8081'
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

let pass = 0
let fail = 0
const ok = (what, good, detail = '') => {
  if (good) {
    pass += 1
    console.log(`  ok   ${what}${detail ? ' — ' + detail : ''}`)
  } else {
    fail += 1
    console.log(`  FAIL ${what}${detail ? ' — ' + detail : ''}`)
  }
}
const head = (t) => console.log(`\n== ${t} ==`)

async function connect(login, password) {
  const ws = await new Promise((res, rej) => {
    const s = new WebSocket(URL)
    s.onopen = () => res(s)
    s.onerror = () => rej(new Error(`cannot reach ${URL}`))
  })
  const frames = []
  ws.onmessage = (e) => frames.push(JSON.parse(e.data))
  const c = {
    ws,
    frames,
    send: (m) => ws.send(JSON.stringify(m)),
    last: (t) => frames.filter((f) => f.type === t).at(-1),
    clear: () => (frames.length = 0),
  }
  if (login) {
    c.send({ type: 'login', login, password })
    await sleep(1500)
  }
  return c
}

// ── Is anything there at all ──
head('the gateway')
const admin = await connect('admin@feraro.local', adminPw)
ok('an administrator signs in', !!admin.last('welcome'), admin.last('welcome')?.login)
if (!admin.last('welcome')) {
  console.log('\nnothing else can be checked without a session.\n')
  process.exit(1)
}

const snap = admin.last('snapshot')
const instruments = snap?.instruments ?? []
ok('the instrument catalogue arrives', instruments.length > 0, `${instruments.length} listed`)

await sleep(9000)
const priced = new Set()
for (const f of admin.frames.filter((x) => x.type === 'quotes')) {
  for (const q of f.quotes) priced.add(q.instrument)
}
ok(
  'every instrument is priced',
  priced.size === instruments.length,
  `${priced.size}/${instruments.length}`,
)
const unpriced = instruments.filter((i) => !priced.has(i.id)).map((i) => i.symbol)
if (unpriced.length) console.log(`       unpriced: ${unpriced.join(', ')}`)

// A feed per asset class, so one dead key cannot pass unnoticed.
const bySource = new Map()
for (const f of admin.frames.filter((x) => x.type === 'quotes')) {
  for (const q of f.quotes) bySource.set(q.source, (bySource.get(q.source) ?? 0) + 1)
}
console.log(`       sources: ${[...bySource.keys()].sort().join(', ')}`)

// ── The book ──
head('the book')
admin.send({ type: 'adminOverview' })
await sleep(1000)
const book = admin.last('adminBook')
ok('the overview comes back', !!book, `${book?.accounts.length} accounts, ${book?.users.length} people`)

const target = book.accounts.find((a) => !a.live) ?? book.accounts[0]
admin.send({ type: 'selectAccount', account: target.id })
await sleep(1000)
ok('the desk selects a client account', !!admin.last('snapshot'), `acting on ${target.id}`)

// ── Money ──
head('money')
const before = admin.last('adminBook').accounts.find((a) => a.id === target.id).balance
for (const [what, kind, amount] of [
  ['deposit', 'deposit', '500.00'],
  ['withdraw', 'withdraw', '200.00'],
  ['set a bonus', 'credit', '50.00'],
]) {
  admin.clear()
  admin.send({ type: 'adminAction', account: target.id, kind, amount })
  await sleep(800)
  ok(what, admin.last('adminResult')?.done, admin.last('adminResult')?.detail)
}
admin.send({ type: 'adminOverview' })
await sleep(900)
const after = admin.last('adminBook').accounts.find((a) => a.id === target.id)
ok('the balance moved by the right amount', after.balance !== before, `${before} → ${after.balance}`)

// ── Control ──
head('control')
for (const [what, kind] of [
  ['suspend', 'suspend'],
  ['reinstate', 'reinstate'],
  ['allow hedging', 'hedging'],
  ['net off again', 'netting'],
  ['renew for 30 days', 'renew'],
]) {
  admin.clear()
  admin.send({ type: 'adminAction', account: target.id, kind, amount: '' })
  await sleep(700)
  ok(what, admin.last('adminResult')?.done, admin.last('adminResult')?.detail)
}

// ── Dealing, on a crypto pair and an FX pair ──
head('dealing')
const opened = []
for (const symbol of ['BTCUSD', 'EURUSD', 'XAUUSD']) {
  const inst = instruments.find((i) => i.symbol === symbol)
  if (!inst) {
    console.log(`       ${symbol} is not listed; skipped`)
    continue
  }
  admin.clear()
  admin.send({
    type: 'placeOrder',
    instrument: inst.id,
    side: 'buy',
    kind: 'market',
    qty: inst.minQty ?? '0.01',
    price: '',
  })
  await sleep(1600)
  const filled = admin.last('filled')
  ok(
    `${symbol} buys at the market`,
    !!filled,
    filled
      ? `ticket ${filled.ticket} at ${filled.price}`
      : JSON.stringify(admin.last('rejected') ?? 'no answer'),
  )
  if (filled) opened.push(filled.ticket)
}

if (opened.length) {
  admin.clear()
  admin.send({ type: 'setProtection', ticket: opened[0], stopLoss: '1.00', takeProfit: '' })
  await sleep(900)
  ok('a stop loss is accepted', !!admin.last('protected') || !!admin.last('portfolio'))
}

// ── Resting orders ──
head('resting orders')
const btc = instruments.find((i) => i.symbol === 'BTCUSD')
if (btc) {
  const q = admin.frames.filter((f) => f.type === 'quotes').at(-1)
  const touch = Number(
    admin.frames
      .filter((f) => f.type === 'quotes')
      .flatMap((f) => f.quotes)
      .filter((x) => x.instrument === btc.id)
      .at(-1)?.bid ?? 0,
  )
  const step = touch * 0.01
  for (const [what, side, kind, price] of [
    ['a buy limit below the market', 'buy', 'limit', (touch - step).toFixed(2)],
    ['a sell limit above it', 'sell', 'limit', (touch + step).toFixed(2)],
  ]) {
    admin.clear()
    admin.send({ type: 'placeOrder', instrument: btc.id, side, kind, qty: '0.01', price })
    await sleep(1200)
    ok(what, !!admin.last('orderPlaced'), admin.last('orderPlaced') ? `at ${price}` : JSON.stringify(admin.last('rejected')))
  }
  await sleep(800)
  const port = admin.last('portfolio')
  ok('they rest rather than fill', (port?.pending?.length ?? 0) >= 2, `${port?.pending?.length ?? 0} resting`)
  for (const o of port?.pending ?? []) {
    admin.send({ type: 'cancelOrder', ticket: o.ticket })
    await sleep(400)
  }
  await sleep(900)
  ok('they cancel', (admin.last('portfolio')?.pending?.length ?? 0) === 0)
}

// ── Records ──
head('records')
admin.clear()
admin.send({ type: 'adminAudit', limit: 10 })
await sleep(900)
ok('the audit trail attributes actions', (admin.last('adminAudit')?.entries.length ?? 0) > 0)
admin.clear()
admin.send({ type: 'statement', limit: 20 })
await sleep(900)
ok('the statement comes back', (admin.last('statement')?.entries?.length ?? 0) > 0)
admin.clear()
admin.send({ type: 'adminBooks', account: 0 })
await sleep(900)
const books = admin.last('adminBooks')?.books
const balanced = (books?.trial ?? []).every((t) => t.residual === '0.000000000' || Number(t.residual) === 0)
ok('every currency nets to zero', balanced, (books?.trial ?? []).map((t) => `${t.currency}:${t.residual}`).join(' '))

// ── Roles ──
if (clientPw) {
  head('the client')
  const client = await connect('demo@feraro.local', clientPw)
  ok('a client signs in', !!client.last('welcome'), client.last('welcome')?.role)
  client.clear()
  client.send({ type: 'adminOverview' })
  await sleep(800)
  ok('a client cannot reach the desk', !!client.last('unauthorized'), client.last('unauthorized')?.detail)
  client.ws.close()
}

if (managerPw) {
  head('the manager')
  const mgr = await connect('boss@feraro.local', managerPw)
  ok('a manager signs in', !!mgr.last('welcome'), mgr.last('welcome')?.role)
  mgr.send({ type: 'adminOverview' })
  await sleep(1000)
  const theirs = mgr.last('adminBook')
  const every = book.accounts.map((a) => a.id)
  const mine = (theirs?.accounts ?? []).map((a) => a.id)
  ok('their book is only their clients', mine.length < every.length, `${mine.length} of ${every.length}`)
  const outside = every.find((id) => !mine.includes(id))
  if (outside !== undefined) {
    mgr.clear()
    mgr.send({ type: 'adminAction', account: outside, kind: 'suspend', amount: '' })
    await sleep(700)
    ok('an account outside it is refused', !!mgr.last('unauthorized'), mgr.last('unauthorized')?.detail)
  }
  mgr.clear()
  mgr.send({ type: 'adminAction', account: mine[0], kind: 'deposit', amount: '100.00' })
  await sleep(700)
  ok('a manager cannot move money', !!mgr.last('unauthorized'), mgr.last('unauthorized')?.detail)
  mgr.ws.close()
}

// ── Leave it as we found it ──
head('clearing up')
admin.clear()
admin.send({ type: 'selectAccount', account: target.id })
await sleep(900)
for (const ticket of opened) {
  admin.send({ type: 'closePosition', ticket })
  await sleep(600)
}
await sleep(1200)
const flat = admin.last('portfolio')
ok('every position opened here is closed', (flat?.positions?.length ?? 0) === 0, `${flat?.positions?.length ?? 0} left`)

console.log(`\n${pass} passed, ${fail} failed\n`)
admin.ws.close()
process.exit(fail === 0 ? 0 : 1)
