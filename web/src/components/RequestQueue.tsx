/**
 * Every way money moves, in one queue, decided one way.
 *
 * # Why one table and not six
 *
 * A deposit, a withdrawal, a bonus granted or taken back, insurance set
 * aside or released: six movements, and a desk that approves each of them
 * on its own screen is a desk that forgets one. They share a state machine
 * — pending, then approved or refused, exactly once — so they share a
 * screen, and the queue length is the only number anybody has to watch.
 *
 * # Why the confirmation changes wording
 *
 * The mistake each kind invites is different, and a generic "Are you sure?"
 * prevents none of them. A deposit is wrong when the desk credits money it
 * has not *seen*; a withdrawal is wrong when the desk marks it paid before
 * *sending* it. So the confirmation asks about the thing the operator has
 * to have done, by name, and the button says it too.
 *
 * Nothing here moves money on its own. Approving raises an ordinary
 * journaled movement with the operator's name on it.
 */

import { useState } from 'react'
import { KIND_WORD, MONEY_KINDS } from '../feed/gateway'
import type { DepositRequest, MoneyKind } from '../feed/gateway'
import { Pill } from './primitives'

/** What each payment rail is called on screen. */
const RAIL_WORD: Record<string, string> = {
  crypto: 'Crypto',
  upi: 'UPI',
  bank: 'Bank',
}

const WHEN = new Intl.DateTimeFormat(undefined, {
  day: '2-digit',
  month: 'short',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

/**
 * What the operator must have done before approving, per kind.
 *
 * `check` goes in the confirmation, so it is phrased as a question about a
 * thing already done, not about an intention.
 */
const DECIDE: Readonly<
  Record<MoneyKind, { readonly go: string; readonly check: (r: DepositRequest) => string }>
> = {
  deposit: {
    go: 'Money arrived',
    check: (r) =>
      `Have you SEEN ${r.amount} arrive from ${r.login ?? 'this client'}? This credits account ${r.account}.`,
  },
  withdrawal: {
    go: 'Money sent',
    check: (r) =>
      `Have you SENT ${r.amount} to ${r.methodLabel || 'their saved method'}? This takes it out of account ${r.account}.`,
  },
  bonusIn: {
    go: 'Grant bonus',
    check: (r) =>
      `Add ${r.amount} of bonus to account ${r.account}? It supports margin and is not theirs to withdraw.`,
  },
  bonusOut: {
    go: 'Take bonus back',
    check: (r) =>
      `Take ${r.amount} of bonus off account ${r.account}? Their free margin falls by it, which can stop a position out.`,
  },
  insuranceIn: {
    go: 'Set aside',
    check: (r) =>
      `Move ${r.amount} of account ${r.account}'s own money out of the market's reach? It stops supporting margin.`,
  },
  insuranceOut: {
    go: 'Release',
    check: (r) => `Release ${r.amount} back into account ${r.account}'s balance?`,
  },
}

/** Why a request is refused, per kind. Chosen, so the reasons stay countable. */
const WHY: Readonly<Record<MoneyKind, string>> = {
  deposit: 'nothing received',
  withdrawal: 'could not send',
  bonusIn: 'not eligible',
  bonusOut: 'left in place',
  insuranceIn: 'not eligible',
  insuranceOut: 'left in place',
}

export interface RequestQueueProps {
  readonly requests: readonly DepositRequest[]
  /** False for a manager: deciding money is an administrator's. */
  readonly mayDecide: boolean
  onSettle(id: number, approve: boolean, detail: string): void
  /** Called after a decision, to pull the queue and the book again. */
  onChanged(): void
}

export function RequestQueue({
  requests,
  mayDecide,
  onSettle,
  onChanged,
}: RequestQueueProps) {
  /** Null is every kind. Filtering is per kind because the desks differ:
   *  whoever pays withdrawals is rarely whoever grants bonuses. */
  const [only, setOnly] = useState<MoneyKind | null>(null)
  const [waitingOnly, setWaitingOnly] = useState(true)

  const shown = requests.filter(
    (r) => (only === null || r.kind === only) && (!waitingOnly || r.stage === 'pending'),
  )
  const waiting = (kind: MoneyKind) =>
    requests.filter((r) => r.kind === kind && r.stage === 'pending').length

  const decide = (r: DepositRequest, approve: boolean) => {
    if (approve) {
      if (!window.confirm(DECIDE[r.kind].check(r))) return
      onSettle(r.id, true, 'checked at the desk')
    } else {
      const why = window.prompt('Why is this refused? The client is told.', WHY[r.kind])
      if (why === null) return
      onSettle(r.id, false, why)
    }
    // The gateway answers the decision, not the queue; the queue is pulled.
    window.setTimeout(onChanged, 250)
  }

  return (
    <div className="rq">
      <div className="rq-filter">
        <button
          type="button"
          className={only === null ? 'on' : undefined}
          onClick={() => setOnly(null)}
        >
          All
          <span className="rq-count">
            {requests.filter((r) => r.stage === 'pending').length}
          </span>
        </button>
        {MONEY_KINDS.map((k) => (
          <button
            key={k}
            type="button"
            className={only === k ? 'on' : undefined}
            onClick={() => setOnly(k)}
          >
            {KIND_WORD[k]}
            {waiting(k) > 0 ? <span className="rq-count">{waiting(k)}</span> : null}
          </button>
        ))}
        <label className="rq-waiting">
          <input
            type="checkbox"
            checked={waitingOnly}
            onChange={(e) => setWaitingOnly(e.target.checked)}
          />
          waiting only
        </label>
      </div>

      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th style={{ width: 54 }}>#</th>
              <th style={{ width: 130 }}>When</th>
              <th style={{ width: 104 }}>Kind</th>
              <th style={{ width: 66 }}>Account</th>
              <th>Client</th>
              <th style={{ width: 150 }}>Method</th>
              <th className="num" style={{ width: 110 }}>
                Amount
              </th>
              <th style={{ width: 160 }}>Reference</th>
              <th style={{ width: 88 }}>State</th>
              <th style={{ width: 190 }} />
            </tr>
          </thead>
          <tbody>
            {shown.map((r) => (
              <tr key={r.id}>
                <td className="mono dim">{r.id}</td>
                <td className="mono dim">{WHEN.format(new Date(r.at))}</td>
                <td>
                  <span className={`rq-kind rq-${r.pot}`}>{KIND_WORD[r.kind]}</span>
                </td>
                <td className="mono">{r.account}</td>
                <td>{r.login ?? <span className="dim">unknown</span>}</td>
                <td>
                  {r.methodLabel ? (
                    <>
                      <span className="dim">{RAIL_WORD[r.rail] ?? r.rail}</span>{' '}
                      {r.methodLabel}
                    </>
                  ) : (
                    <span className="dim">the desk</span>
                  )}
                </td>
                {/* A claim, not an agreed figure, so it is never formatted
                    as though the platform had checked it. */}
                <td className="num mono">{r.amount}</td>
                <td className="mono dim">{r.reference || '—'}</td>
                <td>
                  {r.stage === 'pending' ? (
                    <Pill tone="sell">waiting</Pill>
                  ) : r.stage === 'credited' ? (
                    <Pill tone="ok">done</Pill>
                  ) : (
                    <span className="dim">refused</span>
                  )}
                </td>
                <td>
                  {r.stage !== 'pending' ? (
                    <span className="dim">{r.detail}</span>
                  ) : (
                    <div className="row" style={{ gap: 4 }}>
                      <button
                        type="button"
                        className="mgr-go"
                        disabled={!mayDecide}
                        title={
                          mayDecide
                            ? DECIDE[r.kind].check(r)
                            : 'Deciding money is an administrator’s'
                        }
                        onClick={() => decide(r, true)}
                      >
                        {DECIDE[r.kind].go}
                      </button>
                      <button
                        type="button"
                        className="mgr-mini bad"
                        disabled={!mayDecide}
                        onClick={() => decide(r, false)}
                      >
                        Refuse
                      </button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {shown.length === 0 ? (
          <p className="table-empty dim">
            {waitingOnly ? 'Nothing is waiting.' : 'No requests.'}
          </p>
        ) : (
          <p className="mgr-note">
            Nothing here has moved any money yet. Approving raises an ordinary
            journaled movement with your name on it — so{' '}
            <strong>check the bank, the wallet, or the account first</strong>, and only
            then decide.
          </p>
        )}
      </div>
    </div>
  )
}
