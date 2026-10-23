/**
 * Setting up an authenticator on a desk login.
 *
 * # Why a desk login and not every login
 *
 * This is the login that can take money out of a client's account. A
 * password is one thing, and one thing can be read over a shoulder,
 * phished, or reused from somewhere it was already lost. A client cannot
 * move money without the desk agreeing, so making them carry a phone to
 * trade would cost the platform clients and buy nothing.
 *
 * # Why the secret is shown once
 *
 * After the enrolment frame the platform keeps only what it needs to check
 * codes. If it could show the secret again, any session on this login could
 * read the second factor out — which would make it a second copy of the
 * first one rather than a second factor. Somebody who loses the phone
 * enrols again; they do not recover it.
 *
 * # Why there is no QR code
 *
 * Fetching one is out: a page that asks an image service to render a secret
 * has just told that service the secret. Drawing one means a QR encoder,
 * and a hand-written one cannot be properly tested here without also
 * writing a decoder — an encoder that is subtly wrong produces a square
 * that scans to nothing, and the person blames the platform rather than the
 * code. Typing the secret in works in every app, every time.
 *
 * A QR code is still the better way to do this, and it is on the roadmap
 * with a tested encoder behind it.
 */

import { useState } from 'react'

export interface SecondFactorProps {
  /** True when one is already working on this login. */
  readonly on: boolean
  /** The secret from a fresh enrolment, or null before one is started. */
  readonly secret: {
    readonly secret: string
    readonly grouped: string
    readonly uri: string
  } | null
  onBegin(): void
  onConfirm(code: string): void
  onStop(code: string): void
}

export function SecondFactor({
  on,
  secret,
  onBegin,
  onConfirm,
  onStop,
}: SecondFactorProps) {
  const [code, setCode] = useState('')
  const [showSecret, setShowSecret] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [copied, setCopied] = useState(false)

  const digits = (text: string) => text.replace(/\D/g, '').slice(0, 6)

  if (on && !stopping) {
    return (
      <div className="sf">
        <p className="sf-state sf-good">
          <span className="sf-tick" aria-hidden="true">
            ✓
          </span>
          Two-factor is on. Every sign-in to this login asks for a code —
          including from this browser.
        </p>
        <p className="sf-says">
          If you lose the phone, another administrator turns this off for you.
          There is no recovery code: one written down somewhere is a password
          with extra steps.
        </p>
        <button type="button" className="sf-plain" onClick={() => setStopping(true)}>
          Turn it off
        </button>
      </div>
    )
  }

  if (stopping) {
    return (
      <div className="sf">
        <p className="sf-state sf-warn">
          Turning two-factor off leaves this login on a password alone.
        </p>
        <label className="sf-field">
          <span className="cap">A code from the authenticator</span>
          <input
            className="sf-code mono"
            value={code}
            onChange={(e) => setCode(digits(e.target.value))}
            placeholder="000000"
            inputMode="numeric"
            autoComplete="one-time-code"
            spellCheck={false}
          />
        </label>
        <p className="sf-says">
          A code and not the password: this session is already signed in, so
          asking for the password again would prove nothing a stolen session
          could not also prove. A code proves the phone is here, which is the
          thing being removed.
        </p>
        <div className="sf-feet">
          <button
            type="button"
            className="sf-plain"
            onClick={() => {
              setStopping(false)
              setCode('')
            }}
          >
            Keep it on
          </button>
          <button
            type="button"
            className="sf-go sf-danger"
            disabled={code.length !== 6}
            onClick={() => {
              onStop(code)
              setCode('')
              setStopping(false)
            }}
          >
            Turn it off
          </button>
        </div>
      </div>
    )
  }

  if (secret === null) {
    return (
      <div className="sf">
        <p className="sf-state sf-warn">
          <span className="sf-bang" aria-hidden="true">
            !
          </span>
          This login can move money and has only a password.
        </p>
        <p className="sf-says">
          You will need an authenticator app — Google Authenticator, Authy, 1Password,
          or any other. Have it open before you start: the secret is shown once and
          not again.
        </p>
        <button type="button" className="sf-go" onClick={onBegin}>
          Set up two-factor
        </button>
      </div>
    )
  }

  return (
    <div className="sf">
      <p className="sf-state">
        Add this to your authenticator, then type the code it shows.
      </p>

      <div className="sf-manual">
        <span className="cap">The secret</span>
        {showSecret ? (
          <>
            {/* In groups of four, because this is read off a screen and
                retyped on a phone. The decoder forgives the spaces. */}
            <code className="sf-secret">{secret.grouped}</code>
            <div className="sf-manual-acts">
              <button
                type="button"
                className="sf-plain"
                onClick={() => {
                  // Clipboard access can be refused. The secret is on
                  // screen either way, so this is a convenience failing
                  // rather than the thing failing.
                  navigator.clipboard?.writeText(secret.secret).then(
                    () => setCopied(true),
                    () => setCopied(false),
                  )
                }}
              >
                {copied ? 'Copied' : 'Copy'}
              </button>
              {/* On a phone this opens the authenticator with everything
                  filled in. On a desktop it does nothing, which is why the
                  secret above is the main way in rather than the fallback. */}
              <a className="sf-plain" href={secret.uri}>
                Open in an app
              </a>
            </div>
          </>
        ) : (
          <button
            type="button"
            className="sf-plain"
            onClick={() => setShowSecret(true)}
          >
            Show it
          </button>
        )}
      </div>

      <label className="sf-field">
        <span className="cap">The code the app is showing</span>
        <input
          className="sf-code mono"
          value={code}
          onChange={(e) => setCode(digits(e.target.value))}
          placeholder="000000"
          inputMode="numeric"
          autoComplete="one-time-code"
          spellCheck={false}
        />
      </label>

      <p className="sf-says">
        Nothing is on until a code from it works. If the app and this platform
        disagree, the phone's clock is out — that is what two-factor is sensitive
        to, and it is the usual cause.
      </p>

      <button
        type="button"
        className="sf-go"
        disabled={code.length !== 6}
        onClick={() => {
          onConfirm(code)
          setCode('')
        }}
      >
        Turn it on
      </button>
    </div>
  )
}
