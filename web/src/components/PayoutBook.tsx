/**
 * Where a client's money may be sent, kept by the client.
 *
 * # Why these are saved rather than typed each time
 *
 * A wallet address typed in a hurry is the one mistake on this platform
 * that cannot be undone: money sent to a wrong chain address is gone, and
 * no journal replay brings it back. So an address is typed once, read back
 * in full, and reused — and the withdrawal form never offers a free-text
 * destination.
 *
 * It is also what lets the desk refuse a suspicious change rather than a
 * suspicious payment: a new address appearing an hour before a withdrawal
 * is visible as its own event.
 *
 * # Why one is preferred
 *
 * The common case is one place, used every time. Marking it means the
 * withdrawal form opens on the right destination, so the client confirms a
 * choice instead of making one.
 */

import { useState } from 'react'
import type { PayoutMethod, Rail } from '../feed/gateway'

const RAILS: readonly { readonly value: Rail; readonly label: string; readonly hint: string }[] =
  [
    { value: 'crypto', label: 'Crypto', hint: 'wallet address, with the network' },
    { value: 'upi', label: 'UPI', hint: 'your UPI id' },
    { value: 'bank', label: 'Bank transfer', hint: 'account number and IFSC or IBAN' },
  ]

interface Draft {
  readonly id: number | null
  readonly rail: Rail
  readonly label: string
  readonly address: string
  readonly note: string
  readonly preferred: boolean
}

const EMPTY: Draft = {
  id: null,
  rail: 'bank',
  label: '',
  address: '',
  note: '',
  preferred: false,
}

export interface PayoutBookProps {
  readonly payouts: readonly PayoutMethod[]
  onSave(m: Draft): void
  onForget(id: number): void
}

export function PayoutBook({ payouts, onSave, onForget }: PayoutBookProps) {
  const [draft, setDraft] = useState<Draft | null>(null)

  const rail = RAILS.find((r) => r.value === draft?.rail) ?? RAILS[2]!
  const ready =
    draft !== null && draft.label.trim() !== '' && draft.address.trim() !== ''

  return (
    <div className="pb">
      {payouts.length === 0 && draft === null ? (
        <p className="pb-none">
          Nowhere saved yet. Add the account or wallet you want to be paid to, and it
          will be offered whenever you withdraw.
        </p>
      ) : null}

      <div className="pb-list">
        {payouts.map((p) => (
          <div key={p.id} className={`pb-row${p.preferred ? ' pb-pref' : ''}`}>
            <span className="pb-rail">{RAILS.find((r) => r.value === p.rail)?.label}</span>
            <span className="pb-label">{p.label}</span>
            {/* In full, never truncated: a shortened address is one the
                client cannot check, and checking it is the whole point. */}
            <code className="pb-addr">{p.address}</code>
            {p.note ? <span className="pb-note dim">{p.note}</span> : null}
            <span className="pb-acts">
              {p.preferred ? (
                <span className="pb-is">usual</span>
              ) : (
                <button
                  type="button"
                  className="pb-mini"
                  onClick={() => onSave({ ...p, preferred: true })}
                >
                  Make usual
                </button>
              )}
              <button
                type="button"
                className="pb-mini"
                onClick={() => setDraft({ ...p })}
              >
                Change
              </button>
              <button
                type="button"
                className="pb-mini bad"
                onClick={() => {
                  if (
                    window.confirm(
                      `Forget ${p.label}? Nothing already sent to it is affected.`,
                    )
                  )
                    onForget(p.id)
                }}
              >
                Forget
              </button>
            </span>
          </div>
        ))}
      </div>

      {draft === null ? (
        <button type="button" className="pb-add" onClick={() => setDraft(EMPTY)}>
          Add somewhere to be paid
        </button>
      ) : (
        <div className="pb-form">
          <div className="pb-rails">
            {RAILS.map((r) => (
              <button
                key={r.value}
                type="button"
                className={draft.rail === r.value ? 'on' : undefined}
                onClick={() => setDraft({ ...draft, rail: r.value })}
              >
                {r.label}
              </button>
            ))}
          </div>
          <label>
            <span className="cap">What to call it</span>
            <input
              value={draft.label}
              onChange={(e) => setDraft({ ...draft, label: e.target.value })}
              placeholder="HDFC savings"
              spellCheck={false}
            />
          </label>
          <label>
            <span className="cap">Where it goes</span>
            <input
              className="mono"
              value={draft.address}
              onChange={(e) => setDraft({ ...draft, address: e.target.value })}
              placeholder={rail.hint}
              spellCheck={false}
            />
          </label>
          <label>
            <span className="cap">Anything else the desk needs</span>
            <input
              value={draft.note}
              onChange={(e) => setDraft({ ...draft, note: e.target.value })}
              placeholder="network, branch, holder name"
              spellCheck={false}
            />
          </label>
          <label className="pb-check">
            <input
              type="checkbox"
              checked={draft.preferred}
              onChange={(e) => setDraft({ ...draft, preferred: e.target.checked })}
            />
            Use this one unless I say otherwise
          </label>
          <div className="pb-feet">
            <button type="button" className="pb-cancel" onClick={() => setDraft(null)}>
              Cancel
            </button>
            <button
              type="button"
              className="pb-go"
              disabled={!ready}
              onClick={() => {
                onSave({
                  ...draft,
                  label: draft.label.trim(),
                  address: draft.address.trim(),
                  note: draft.note.trim(),
                })
                setDraft(null)
              }}
            >
              {draft.id === null ? 'Save it' : 'Save changes'}
            </button>
          </div>
          <p className="pb-warn">
            Read the address back before you save it. Money sent to a wrong address
            cannot be recovered by anyone, including us.
          </p>
        </div>
      )}
    </div>
  )
}
