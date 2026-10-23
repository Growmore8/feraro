/**
 * Everything a client does with their money that is not a trade.
 *
 * Three things, behind three tabs, because they are done at different
 * times: pay in now, ask to be paid out later, and keep the places you are
 * paid to. Splitting them across three menu items would hide the one a
 * client is looking for; putting them on one scrolling page would put a
 * withdrawal form under the finger of somebody trying to deposit.
 *
 * # Nothing here moves money
 *
 * There is no payment provider. Paying in shows the details the desk
 * published and lets the client say they have sent it. Asking to be paid
 * out joins the desk's queue. In both directions a person at the desk
 * checks, and the account changes then — not when a button is pressed.
 *
 * The screen says so in those words, because a client who believes the
 * button paid them comes back angry in ten minutes, and one who was told
 * the desk has to check will wait.
 */

import { useState } from 'react'
import { KIND_WORD } from '../feed/gateway'
import type { DepositRequest, PaymentMethod, PayoutMethod, Rail } from '../feed/gateway'
import type { Money } from '../lib/money'
import { DepositSheet } from './DepositSheet'
import { PayoutBook } from './PayoutBook'
import { WithdrawForm } from './WithdrawForm'

const WHEN = new Intl.DateTimeFormat(undefined, {
  day: '2-digit',
  month: 'short',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

export type WalletTab = 'in' | 'out' | 'where'

export interface WalletProps {
  readonly methods: readonly PaymentMethod[]
  readonly payouts: readonly PayoutMethod[]
  readonly mine: readonly DepositRequest[]
  /** The account money would move on. */
  readonly account: number
  /** False on a demo account, where there is nothing real to send. */
  readonly live: boolean
  /** Outstanding bonus, which must be cleared before anything may leave. */
  readonly credit: Money
  readonly freeMargin: Money
  readonly insurance: Money
  readonly start?: WalletTab
  onDeposit(method: number, amount: string, reference: string): void
  onWithdraw(payout: number, amount: string, note: string): void
  onSavePayout(m: {
    id: number | null
    rail: Rail
    label: string
    address: string
    note: string
    preferred: boolean
  }): void
  onForgetPayout(id: number): void
}

export function Wallet({
  methods,
  payouts,
  mine,
  account,
  live,
  credit,
  freeMargin,
  insurance,
  start = 'in',
  onDeposit,
  onWithdraw,
  onSavePayout,
  onForgetPayout,
}: WalletProps) {
  const [tab, setTab] = useState<WalletTab>(start)

  const waiting = mine.filter((r) => r.stage === 'pending').length

  return (
    <div className="wal">
      <div className="wal-tabs">
        <button
          type="button"
          className={tab === 'in' ? 'on' : undefined}
          onClick={() => setTab('in')}
        >
          Pay in
        </button>
        <button
          type="button"
          className={tab === 'out' ? 'on' : undefined}
          onClick={() => setTab('out')}
        >
          Withdraw
        </button>
        <button
          type="button"
          className={tab === 'where' ? 'on' : undefined}
          onClick={() => setTab('where')}
        >
          Where to send
          {payouts.length > 0 ? <span className="wal-count">{payouts.length}</span> : null}
        </button>
      </div>

      {tab === 'in' ? (
        <DepositSheet
          methods={methods}
          mine={mine.filter((r) => r.kind === 'deposit')}
          account={account}
          onRequest={onDeposit}
        />
      ) : null}

      {tab === 'out' ? (
        <WithdrawForm
          payouts={payouts}
          account={account}
          live={live}
          credit={credit}
          freeMargin={freeMargin}
          insurance={insurance}
          onWithdraw={onWithdraw}
          onAddPayout={() => setTab('where')}
        />
      ) : null}

      {tab === 'where' ? (
        <PayoutBook payouts={payouts} onSave={onSavePayout} onForget={onForgetPayout} />
      ) : null}

      {waiting > 0 ? (
        <div className="wal-waiting">
          <span className="cap">Waiting on the desk</span>
          {mine
            .filter((r) => r.stage === 'pending')
            .slice(0, 6)
            .map((r) => (
              <div key={r.id} className="wal-wrow">
                <span className="dim">{KIND_WORD[r.kind]}</span>
                <span className="mono">{r.amount}</span>
                <span className="dim">{WHEN.format(new Date(r.at))}</span>
              </div>
            ))}
        </div>
      ) : null}
    </div>
  )
}
