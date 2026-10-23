/**
 * How a client pays money in.
 *
 * # Nothing here moves money
 *
 * There is no payment provider. This shows the details the desk published —
 * a wallet, a UPI id, a bank account — and lets the client say they have
 * paid. The account is credited when somebody at the desk looks at the bank
 * or the wallet, sees the money, and deposits it.
 *
 * The screen says so, in those words. A client who believes the button
 * credited them comes back angry in ten minutes; one who was told the desk
 * has to check will wait.
 */

import { useState } from 'react'
import type { DepositRequest, PaymentMethod, Rail } from '../feed/gateway'

const RAIL_WORD: Record<Rail, string> = {
  crypto: 'Crypto',
  upi: 'UPI',
  bank: 'Bank transfer',
}

const WHEN = new Intl.DateTimeFormat(undefined, {
  day: '2-digit',
  month: 'short',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

export interface DepositSheetProps {
  readonly methods: readonly PaymentMethod[]
  readonly mine: readonly DepositRequest[]
  /** The account the money would be credited to. */
  readonly account: number
  onRequest(method: number, amount: string, reference: string): void
}

export function DepositSheet({ methods, mine, account, onRequest }: DepositSheetProps) {
  const [chosen, setChosen] = useState<number | null>(methods[0]?.id ?? null)
  const [amount, setAmount] = useState('')
  const [reference, setReference] = useState('')
  const [copied, setCopied] = useState(false)

  const method = methods.find((m) => m.id === chosen) ?? methods[0] ?? null
  const ready = method !== null && amount.trim() !== ''

  if (methods.length === 0) {
    return (
      <p className="dep-none">
        The desk has not published a way to pay in yet. Ask them, and it will appear
        here.
      </p>
    )
  }

  return (
    <div className="dep">
      <div className="dep-rails">
        {methods.map((m) => (
          <button
            key={m.id}
            type="button"
            className={m.id === method?.id ? 'on' : undefined}
            onClick={() => {
              setChosen(m.id)
              setCopied(false)
            }}
          >
            <span className="dep-rail">{RAIL_WORD[m.rail]}</span>
            <span className="dep-label">{m.label}</span>
          </button>
        ))}
      </div>

      {method ? (
        <>
          <div className="dep-to">
            <span className="cap">Send to</span>
            <code className="dep-address">{method.address}</code>
            <button
              type="button"
              className="dep-copy"
              onClick={() => {
                // Clipboard access can be refused. The address is on screen
                // either way, so this is a convenience failing rather than
                // the thing failing.
                navigator.clipboard?.writeText(method.address).then(
                  () => setCopied(true),
                  () => setCopied(false),
                )
              }}
            >
              {copied ? 'Copied' : 'Copy'}
            </button>
          </div>
          {method.note ? <p className="dep-note">{method.note}</p> : null}
        </>
      ) : null}

      <div className="dep-form">
        <label>
          <span className="cap">How much did you send?</span>
          <input
            className="mono"
            value={amount}
            onChange={(e) => setAmount(e.target.value)}
            placeholder="0.00"
            inputMode="decimal"
            spellCheck={false}
          />
        </label>
        <label>
          <span className="cap">
            {method?.rail === 'crypto'
              ? 'Transaction hash'
              : method?.rail === 'upi'
                ? 'UPI reference'
                : 'Transfer reference'}
          </span>
          <input
            className="mono"
            value={reference}
            onChange={(e) => setReference(e.target.value)}
            placeholder="so the desk can find it"
            spellCheck={false}
          />
        </label>
        <button
          type="button"
          className="dep-go"
          disabled={!ready}
          onClick={() => {
            if (!method) return
            onRequest(method.id, amount.trim(), reference.trim())
            setAmount('')
            setReference('')
          }}
        >
          I have sent it
        </button>
      </div>

      <p className="dep-says">
        This tells the desk to look. Account <span className="mono">{account}</span> is
        credited once they have seen the money arrive — not when you press the button.
      </p>

      {mine.length > 0 ? (
        <div className="dep-mine">
          <span className="cap">Your deposits</span>
          {mine.slice(0, 8).map((d) => (
            <div key={d.id} className={`dep-row dep-${d.stage}`}>
              <span className="mono">{d.amount}</span>
              <span className="dim">{d.methodLabel}</span>
              <span className="dim">{WHEN.format(new Date(d.at))}</span>
              <span className="dep-stage">
                {d.stage === 'pending'
                  ? 'waiting to be checked'
                  : d.stage === 'credited'
                    ? 'credited'
                    : `refused — ${d.detail || 'not seen'}`}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}
