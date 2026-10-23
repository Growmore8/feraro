/**
 * The sign-in screen.
 *
 * The gateway answers nothing before a session exists, so this is the whole
 * platform until someone signs in. It carries no account numbers, no symbol
 * list and no prices: there is nothing here to learn by looking at it, which
 * is the point — the page a stranger can reach should teach them nothing.
 *
 * # Two halves
 *
 * The left half says what this is, because a back office is opened by people
 * who were sent a link and need to know they are in the right place. The
 * right half is the form, kept narrow so the eye lands on it. On a phone the
 * left half goes and the form takes the screen.
 *
 * The password never touches state that outlives the attempt, and the form is
 * a real `<form>` so a password manager recognises it and Enter submits.
 */

import { useEffect, useRef, useState } from 'react'
import type { AuthStage } from '../feed/gateway'
import type { Server } from '../feed/servers'
import type { FeedState } from '../feed/types'
import './SignIn.css'

export interface SignInProps {
  /** How far the connection has got. */
  readonly auth: AuthStage
  /** The socket's own state, so a gateway that is down says so. */
  readonly status: FeedState
  /** Why the socket is unhappy, when it is. */
  readonly detail?: string | undefined
  /** Why the last attempt failed, if it did. */
  readonly error: string | null
  onSignIn(login: string, password: string): void
  /** Flips the palette. This page is often the first thing anybody sees. */
  onTheme?: (() => void) | undefined
  /**
   * The servers this site offers.
   *
   * One terminal serves several deployments — a demo server and a live one,
   * a server per region — and they are separate platforms with separate
   * accounts. The choice belongs here, beside the credentials it goes with:
   * discovering it after signing in is too late, because the password has
   * already been sent somewhere.
   */
  readonly servers?: readonly Server[] | undefined
  readonly server?: Server | undefined
  onServer?: ((server: Server) => void) | undefined
}

export function SignIn({
  auth,
  status,
  detail,
  error,
  onSignIn,
  onTheme,
  servers = [],
  server,
  onServer,
}: SignInProps) {
  const [login, setLogin] = useState('')
  const [password, setPassword] = useState('')
  const [reveal, setReveal] = useState(false)
  const loginRef = useRef<HTMLInputElement>(null)

  // The cursor belongs in the first field, and back there after a refusal.
  useEffect(() => {
    if (auth === 'required') loginRef.current?.focus()
  }, [auth, error])

  const reachable = status === 'live'
  const busy = auth === 'signingIn'
  const ready = reachable && !busy && login.trim() !== '' && password !== ''

  const submit = (e: React.FormEvent) => {
    e.preventDefault()
    if (!ready) return
    onSignIn(login.trim(), password)
    // Nothing is kept: a retry is typed again rather than resent from a field
    // nobody can see.
    setPassword('')
    setReveal(false)
  }

  return (
    <div className="signin">
      {/* ── What this is ── */}
      <aside className="signin-pitch">
        <Rings />
        <div className="signin-pitch-body">
          <Mark size={76} />
          <h1 className="signin-head">
            The precision back office
            <br />
            for forex &amp; CFD brokers.
          </h1>
          <ul className="signin-points">
            <Point icon={<ShieldIcon />}>
              Role-based access for every client, manager and desk
            </Point>
            <Point icon={<LockIcon />}>Signed sessions with a full audit trail</Point>
            <Point icon={<ChartIcon />}>
              Real-time market, deal and position oversight
            </Point>
          </ul>
          <p className="signin-seal">
            <span className="signin-dot" aria-hidden="true" />
            Every action journaled, with the name of whoever took it
          </p>
        </div>
      </aside>

      {/* ── The form ── */}
      <main className="signin-right">
        {onTheme ? (
          <button
            type="button"
            className="signin-theme"
            onClick={onTheme}
            aria-label="Switch between light and dark"
            title="Switch between light and dark"
          >
            <MoonIcon />
          </button>
        ) : null}

        <form className="signin-card" onSubmit={submit}>
          <div className="signin-crest">
            <Mark size={54} />
            <span className="signin-est">FERARO</span>
          </div>

          <h2 className="signin-title">Sign in to your dashboard</h2>
          <p className="signin-sub">Back office console</p>

          {servers.length > 1 && server && onServer ? (
            <label className="signin-field">
              <span className="signin-label">Server</span>
              <span className="signin-wrap">
                <ServerIcon />
                <select
                  className="signin-input signin-pick"
                  value={server.url}
                  disabled={busy}
                  onChange={(e) => {
                    const picked = servers.find((s) => s.url === e.target.value)
                    if (picked) onServer(picked)
                  }}
                >
                  {servers.map((s) => (
                    <option key={s.url} value={s.url}>
                      {s.name}
                      {s.note ? ` — ${s.note}` : ''}
                    </option>
                  ))}
                </select>
              </span>
            </label>
          ) : null}

          <label className="signin-field">
            <span className="signin-label">Email address</span>
            <span className="signin-wrap">
              <PersonIcon />
              <input
                ref={loginRef}
                className="signin-input"
                value={login}
                onChange={(e) => setLogin(e.target.value)}
                placeholder="name@example.com"
                autoComplete="username"
                autoCapitalize="off"
                spellCheck={false}
                inputMode="email"
                disabled={busy}
              />
            </span>
          </label>

          <label className="signin-field">
            <span className="signin-label">Password</span>
            <span className="signin-wrap">
              <KeyIcon />
              <input
                className="signin-input"
                type={reveal ? 'text' : 'password'}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="Enter your password"
                autoComplete="current-password"
                disabled={busy}
              />
              <button
                type="button"
                className="signin-eye"
                onClick={() => setReveal((r) => !r)}
                aria-label={reveal ? 'Hide the password' : 'Show the password'}
                title={reveal ? 'Hide the password' : 'Show the password'}
                tabIndex={-1}
              >
                <EyeIcon off={reveal} />
              </button>
            </span>
          </label>

          {error ? (
            <p className="signin-error" role="alert">
              {error}
            </p>
          ) : null}

          {!reachable ? (
            <p className="signin-error" role="alert">
              {detail ?? 'The gateway is not answering.'} Start it and this page will
              reconnect on its own.
            </p>
          ) : null}

          <button type="submit" className="signin-go" disabled={!ready}>
            {busy ? 'Signing in…' : 'Sign in'}
          </button>

          <p className="signin-note">
            Your session ends when this tab closes. There is deliberately no
            &ldquo;keep me signed in&rdquo;: a token that outlives the tab is a token
            somebody else can use.
          </p>
        </form>

        <footer className="signin-foot">
          {server ? (
            <span className="signin-at" title={server.url}>
              <span className={`signin-bead${reachable ? ' on' : ''}`} aria-hidden="true" />
              {server.name}
            </span>
          ) : null}
          <span>© {new Date().getFullYear()} Feraro</span>
          <span className="mono">v0.1.0</span>
        </footer>
      </main>
    </div>
  )
}

function Point({ icon, children }: { icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <li className="signin-point">
      <span className="signin-point-icon" aria-hidden="true">
        {icon}
      </span>
      <span>{children}</span>
    </li>
  )
}

/**
 * The mark.
 *
 * A ledger fold: two leaves, one deep and one gold, meeting at a line. It is
 * drawn rather than loaded, so it needs no network and inherits no colour it
 * was not given.
 */
function Mark({ size }: { size: number }) {
  return (
    <svg
      className="signin-logo"
      width={size}
      height={size * 0.78}
      viewBox="0 0 100 78"
      fill="none"
      aria-hidden="true"
    >
      <path d="M4 4h92L62 74a8 8 0 0 1-14 0L4 4Z" fill="var(--brand-deep)" />
      <path d="M50 40 96 4 62 74a8 8 0 0 1-12 1V40Z" fill="var(--brand-gold)" />
    </svg>
  )
}

/** The quiet orbit behind the pitch. Decoration, and it says so. */
function Rings() {
  return (
    <svg className="signin-rings" viewBox="0 0 600 600" aria-hidden="true">
      <g fill="none" stroke="currentColor" strokeWidth="1">
        <circle cx="300" cy="300" r="120" opacity="0.5" />
        <circle cx="300" cy="300" r="190" opacity="0.35" />
        <circle cx="300" cy="300" r="262" opacity="0.2" />
        <path
          d="M300 38a262 262 0 0 1 185 77"
          stroke="var(--brand-gold)"
          strokeWidth="1.5"
          opacity="0.85"
        />
        <path d="M300 20v44M300 536v44M20 300h44M536 300h44" opacity="0.4" />
      </g>
      <circle cx="485" cy="115" r="7" fill="var(--brand-gold)" />
    </svg>
  )
}

const ShieldIcon = () => (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4">
    <path d="M8 1.5 2.5 4v4c0 3 2.3 5.6 5.5 6.5 3.2-.9 5.5-3.5 5.5-6.5V4L8 1.5Z" />
  </svg>
)

const LockIcon = () => (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4">
    <rect x="3" y="7" width="10" height="7" rx="1" />
    <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
  </svg>
)

const ChartIcon = () => (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4">
    <path d="M2 13h12M4 10.5l3-3.5 2.5 2.5L13 4" />
  </svg>
)

const PersonIcon = () => (
  <svg
    className="signin-icon"
    width="14"
    height="14"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.3"
  >
    <circle cx="8" cy="5.5" r="2.8" />
    <path d="M2.8 14c.7-2.7 2.7-4 5.2-4s4.5 1.3 5.2 4" />
  </svg>
)

const KeyIcon = () => (
  <svg
    className="signin-icon"
    width="14"
    height="14"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.3"
  >
    <rect x="3" y="7" width="10" height="7" rx="1" />
    <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
  </svg>
)

const EyeIcon = ({ off }: { off: boolean }) => (
  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
    <path d="M1.5 8S4 3.8 8 3.8 14.5 8 14.5 8 12 12.2 8 12.2 1.5 8 1.5 8Z" />
    <circle cx="8" cy="8" r="2" />
    {off ? <path d="M2.5 2.5l11 11" /> : null}
  </svg>
)

const ServerIcon = () => (
  <svg
    className="signin-icon"
    width="14"
    height="14"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.3"
  >
    <rect x="2" y="2.5" width="12" height="4.5" rx="1" />
    <rect x="2" y="9" width="12" height="4.5" rx="1" />
    <path d="M4.5 4.75h.01M4.5 11.25h.01" strokeLinecap="round" strokeWidth="1.8" />
  </svg>
)

const MoonIcon = () => (
  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
    <path d="M13.5 9.6A6 6 0 0 1 6.4 2.5a6 6 0 1 0 7.1 7.1Z" />
  </svg>
)
