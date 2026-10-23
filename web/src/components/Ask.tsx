/**
 * The dialog an action opens when it needs a figure.
 *
 * `window.prompt` takes one string with no label, no second field and no
 * context, and on a desk the context is the point: an amount typed against
 * the wrong account is the mistake this screen exists to prevent. So the
 * dialog wears its subject — the account number and whose it is — in the
 * header, beside an icon that says which action this is.
 *
 * Enter submits, Escape cancels, the backdrop cancels, and the first field
 * is focused and selected so a figure already there is replaced by typing
 * rather than appended to.
 */

import { useEffect, useRef, useState } from 'react'

export interface AskField {
  readonly key: string
  readonly label: string
  readonly placeholder?: string
  /** `decimal` for money and prices, `numeric` for a ticket. */
  readonly mode?: 'decimal' | 'numeric' | 'text'
  /** A fixed set of answers, rather than something typed. */
  readonly choices?: readonly { readonly value: string; readonly label: string }[]
  readonly initial?: string
  /** False when the action can proceed without it. */
  readonly required?: boolean
  /** Lays this field beside the previous one rather than under it. */
  readonly beside?: boolean
}

export interface AskProps {
  readonly title: string
  /** Who or what this acts on, under the title. */
  readonly subject?: string | undefined
  /** One glyph, in the disc beside the title. */
  readonly icon?: string | undefined
  /** What the action is about to do, in the words the confirmation uses. */
  readonly note?: string | undefined
  readonly fields: readonly AskField[]
  readonly submitLabel: string
  readonly tone?: 'danger'
  onSubmit(values: Record<string, string>): void
  onCancel(): void
}

export function Ask({
  title,
  subject,
  icon,
  note,
  fields,
  submitLabel,
  tone,
  onSubmit,
  onCancel,
}: AskProps) {
  const first = useRef<HTMLInputElement | HTMLSelectElement>(null)
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries(fields.map((f) => [f.key, f.initial ?? f.choices?.[0]?.value ?? ''])),
  )

  useEffect(() => {
    const el = first.current
    el?.focus()
    // Selecting the text means a figure already there is replaced by typing
    // rather than appended to, which is what a filled-in amount is for.
    if (el instanceof HTMLInputElement) el.select()
  }, [])

  const missing = fields.some(
    (f) => (f.required ?? true) && (values[f.key] ?? '').trim() === '',
  )

  const submit = () => {
    if (missing) return
    onSubmit(values)
  }

  return (
    <div
      className="ask-back"
      onPointerDown={(e) => {
        if (e.target === e.currentTarget) onCancel()
      }}
    >
      <div
        className="ask"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={(e) => {
          if (e.key === 'Escape') onCancel()
          if (e.key === 'Enter') submit()
        }}
      >
        <header className="ask-head">
          {icon ? (
            <span className="ask-icon" aria-hidden="true">
              {icon}
            </span>
          ) : null}
          <span className="ask-who">
            <span className="ask-title">{title}</span>
            {subject ? <span className="ask-subject">{subject}</span> : null}
          </span>
          <button
            type="button"
            className="ask-shut"
            onClick={onCancel}
            aria-label="Close"
            title="Close"
          >
            ×
          </button>
        </header>

        {note ? <p className="ask-note">{note}</p> : null}

        <div className="ask-fields">
          {fields.map((f, i) => (
            <label key={f.key} className={`ask-field${f.beside ? ' ask-beside' : ''}`}>
              <span className="ask-label">{f.label}</span>
              {f.choices ? (
                <select
                  ref={i === 0 ? (first as React.Ref<HTMLSelectElement>) : undefined}
                  className="ask-input"
                  value={values[f.key] ?? ''}
                  onChange={(e) => setValues((v) => ({ ...v, [f.key]: e.target.value }))}
                >
                  {f.choices.map((c) => (
                    <option key={c.value} value={c.value}>
                      {c.label}
                    </option>
                  ))}
                </select>
              ) : (
                <input
                  ref={i === 0 ? (first as React.Ref<HTMLInputElement>) : undefined}
                  className={`ask-input${f.mode === 'text' ? '' : ' mono'}`}
                  value={values[f.key] ?? ''}
                  placeholder={f.placeholder ?? ''}
                  inputMode={f.mode === 'text' ? undefined : (f.mode ?? 'decimal')}
                  spellCheck={false}
                  onChange={(e) => setValues((v) => ({ ...v, [f.key]: e.target.value }))}
                />
              )}
            </label>
          ))}
        </div>

        <footer className="ask-feet">
          <button type="button" className="ask-cancel" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className={`ask-go${tone === 'danger' ? ' ask-danger' : ''}`}
            disabled={missing}
            onClick={submit}
          >
            {submitLabel}
          </button>
        </footer>
      </div>
    </div>
  )
}
