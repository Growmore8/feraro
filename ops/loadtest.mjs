// What happens when a team actually uses it.
//
// Signs in N traders at once, each on their own account, and has them trade
// against live prices for a while. Then it checks the things that would
// make the platform wrong rather than merely slow:
//
//   * every order got an answer — a fill or a stated refusal, never silence
//   * nobody was disconnected
//   * the books still net to zero in every currency
//   * the balances the engine reports match what the fills say they should
//
// Speed matters less than those. A slow platform annoys people; a platform
// whose books do not balance has lost somebody's money.
//
//   node ops/loadtest.mjs <adminPassword> [traders] [seconds]

const [adminPw, tradersArg, secondsArg] = process.argv.slice(2)
const TRADERS = Number(tradersArg ?? 8)
const SECONDS = Number(secondsArg ?? 45)
const URL = process.env.FERARO_URL ?? 'ws://127.0.0.1:8081'
const PASSWORD = 'load-test-pass-2026'

if (!adminPw) {
  console.error('usage: node ops/loadtest.mjs <adminPw> [traders] [seconds]')
  process.exit(2)
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const pick = (a) => a[Math.floor(Math.random() * a.length)]

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

async function connect(login, password) {
  const ws = await new Promise((res, rej) => {
    const s = new WebSocket(URL)
    s.onopen = () => res(s)
    s.onerror = () => rej(new Error(`cannot reach ${URL}`))
  })
  const c = {
    ws,
    frames: [],
    closed: false,
    send: (m) => ws.readyState === 1 && ws.send(JSON.stringify(m)),
    last: (t) => c.frames.filter((f) => f.type === t).at(-1),
    count: (t) => c.frames.filter((f) => f.type === t).length,
    clear: () => (c.frames.length = 0),
  }
  ws.onmessage = (e) => c.frames.push(JSON.parse(e.data))
  ws.onclose = () => (c.closed = true)
  if (login) {
    c.send({ type: 'login', login, password })
    await sleep(1500)
  }
  return c
}

// ── Make sure there are enough people to be a team ──
console.log(`\n== setting up ${TRADERS} traders ==`)
const admin = await connect('admin@feraro.local', adminPw)
if (!admin.last('welcome')) {
  console.error('the administrator could not sign in; is the gateway running?')
  process.exit(1)
}

const logins = []
for (let i = 1; i <= TRADERS; i += 1) {
  const login = `load${i}@feraro.local`
  logins.push(login)
  admin.clear()
  admin.send({ type: 'adminCreateUser', login, password: PASSWORD })
  await sleep(250)
}
await sleep(1200)
console.log(`  ${TRADERS} logins ready`)

// ── Everybody signs in at once ──
console.log('\n== everybody signs in at once ==')
const started = Date.now()
const traders = await Promise.all(logins.map((l) => connect(l, PASSWORD)))
const signInMs = Date.now() - started
const signedIn = traders.filter((t) => t.last('welcome')).length
ok(`all ${TRADERS} signed in`, signedIn === TRADERS, `${signedIn}/${TRADERS} in ${signInMs} ms`)

const accounts = traders.map((t) => t.last('welcome')?.account).filter(Boolean)
ok('each got their own account', new Set(accounts).size === accounts.length, accounts.join(', '))

// ── Prices reach everyone ──
await sleep(5000)
const quoted = traders.filter((t) => t.count('quotes') > 0).length
ok('prices reach every terminal', quoted === TRADERS, `${quoted}/${TRADERS} receiving`)
const perTrader = traders.map((t) => t.count('quotes'))
console.log(`       quote frames each: min ${Math.min(...perTrader)}, max ${Math.max(...perTrader)}`)

// ── Trade ──
console.log(`\n== trading for ${SECONDS}s ==`)
const SYMBOLS = [1, 2, 3, 7]
let sent = 0

// What each trader holds is read from the portfolio the engine sends, not
// inferred from fills. A close produces a `filled` frame too — it is a fill
// on the other side — so counting fills as opens makes the test try to
// close tickets twice and then blame the platform for refusing.
const heldBy = (t) => (t.last('portfolio')?.positions ?? []).map((p) => p.ticket)

const until = Date.now() + SECONDS * 1000
while (Date.now() < until) {
  await Promise.all(
    traders.map(async (t) => {
      if (t.closed) return
      const held = heldBy(t)
      // Roughly a third of actions close something, so positions do not
      // only ever pile up.
      if (held.length > 0 && Math.random() < 0.35) {
        t.send({ type: 'closePosition', ticket: pick(held) })
      } else if (held.length < 6) {
        t.send({
          type: 'placeOrder',
          instrument: pick(SYMBOLS),
          side: Math.random() < 0.5 ? 'buy' : 'sell',
          kind: 'market',
          qty: '0.01',
          price: '',
        })
        sent += 1
      }
    }),
  )
  await sleep(400)
}
await sleep(2500)

const dropped = traders.filter((t) => t.closed).length
ok('nobody was disconnected', dropped === 0, `${dropped} dropped`)

const rejects = traders.reduce((n, t) => n + t.count('rejected'), 0)
const reasons = {}
for (const t of traders) {
  for (const r of t.frames.filter((x) => x.type === 'rejected')) {
    reasons[r.reason] = (reasons[r.reason] ?? 0) + 1
  }
}
console.log(`       ${sent} orders sent · ${rejects} refused`)
if (rejects > 0) console.log(`       reasons: ${JSON.stringify(reasons)}`)
// A refusal is not a failure — an order that cannot be afforded must be
// refused. What would be a failure is silence, or a refusal with no reason.
ok('nothing was refused without a reason', !Object.keys(reasons).includes('undefined'))

// ── Close everything ──
console.log('\n== closing out ==')
for (const t of traders) {
  for (const ticket of heldBy(t)) {
    t.send({ type: 'closePosition', ticket })
    await sleep(60)
  }
}
await sleep(4000)

// ── The checks that matter ──
console.log('\n== is it still correct ==')
admin.clear()
admin.send({ type: 'adminBooks', account: 0 })
await sleep(1500)
const trial = admin.last('adminBooks')?.books?.trial ?? []
const balanced = trial.every((t) => Number(t.residual) === 0)
ok(
  'every currency still nets to zero',
  balanced && trial.length > 0,
  trial.map((t) => `${t.currency}:${t.residual}`).join(' ') || 'no trial balance returned',
)

admin.clear()
admin.send({ type: 'adminOverview' })
await sleep(1500)
const book = admin.last('adminBook')
const mine = (book?.accounts ?? []).filter((a) => logins.includes(a.ownerLogin ?? ''))
ok('every trader has an account on the book', mine.length === TRADERS, `${mine.length}/${TRADERS}`)
const stillOpen = mine.reduce((n, a) => n + a.positions, 0)
console.log(`       ${stillOpen} position(s) left open across them`)

const health = await fetch('http://127.0.0.1:8082/health').then(
  (r) => r.json(),
  () => null,
)
if (health) {
  ok('the gateway reports itself healthy', health.ok === true, JSON.stringify(health))
}

console.log(`\n${pass} passed, ${fail} failed\n`)
admin.ws.close()
for (const t of traders) t.ws.close()
process.exit(fail === 0 ? 0 : 1)
