import { useEffect, useState } from 'react'
import { useGateway } from './feed/useGateway'
import { ManagerTerminal } from './terminals/ManagerTerminal'
import { ClientTerminal } from './terminals/ClientTerminal'
import { MobileTerminal } from './terminals/MobileTerminal'
import { SignIn } from './terminals/SignIn'
import type { Server } from './feed/servers'
import { loadServers, remember, remembered, thisOrigin } from './feed/servers'

/**
 * Which servers this site offers, and which one is chosen.
 *
 * The list is fetched once. Until it arrives the terminal already has a
 * usable answer — the origin that served the page — so the sign-in screen
 * never waits on a file that may not exist.
 */
function useServers(): {
  servers: readonly Server[]
  chosen: Server
  choose: (s: Server) => void
} {
  const [servers, setServers] = useState<readonly Server[]>(() => [thisOrigin()])
  const [chosen, setChosen] = useState<Server>(() => thisOrigin())

  useEffect(() => {
    let live = true
    void loadServers().then((listed) => {
      if (!live) return
      setServers(listed)
      // The one they used last, or the first offered. Never a different
      // server than the one they signed in to yesterday without saying so.
      setChosen(remembered(listed) ?? listed[0] ?? thisOrigin())
    })
    return () => {
      live = false
    }
  }, [])

  return {
    servers,
    chosen,
    choose: (s: Server) => {
      remember(s)
      setChosen(s)
    },
  }
}

/**
 * Below this width the desk layout stops being a layout.
 *
 * It is a width rather than a device test on purpose: a narrow window on a
 * laptop has the same problem as a phone, and a tablet held wide does not.
 */
const PHONE = 820

function usePhone(): boolean {
  const [phone, setPhone] = useState(
    () => typeof window !== 'undefined' && window.innerWidth < PHONE,
  )
  useEffect(() => {
    const query = window.matchMedia(`(max-width: ${PHONE - 1}px)`)
    const look = () => setPhone(query.matches)
    look()
    query.addEventListener('change', look)
    return () => query.removeEventListener('change', look)
  }, [])
  return phone
}

/**
 * Terminal shell.
 *
 * Signing in happens here, above the terminal, because the gateway answers
 * nothing without a session: there is no partly-usable platform to show while
 * somebody is not signed in. The admin terminal mounts here beside the client
 * one and shares the same primitives — see `design/` for what it looks like.
 *
 * The connection is opened once, here, and handed down. Opening it inside the
 * terminal would mean a second socket — and a second session — the moment
 * anything else needs one.
 */
export default function App() {
  const [theme, setTheme] = useState<'light' | 'dark'>('light')
  const { servers, chosen, choose } = useServers()
  // Changing server reopens the socket, because it is a different platform
  // — different accounts, a different journal, and a session that means
  // nothing on the other one.
  const gw = useGateway(chosen.url)
  const phone = usePhone()
  const flip = () => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))

  useEffect(() => {
    document.documentElement.dataset['theme'] = theme
  }, [theme])

  if (gw.auth !== 'signedIn') {
    return (
      <SignIn
        auth={gw.auth}
        status={gw.status}
        detail={gw.detail}
        error={gw.authError}
        onSignIn={gw.signIn}
        onTheme={flip}
        servers={servers}
        server={chosen}
        onServer={choose}
      />
    )
  }

  // A phone gets one screen at a time and a tab bar, whatever the role:
  // four panes at once is the desk layout's whole point, and a phone cannot
  // show four panes.
  if (phone) return <MobileTerminal gw={gw} onTheme={flip} />

  // The desk works one screen: the book, the chart and the deal ticket
  // together. A manager gets the same screen over the clients assigned to
  // them. A client has no other screen to be on.
  const role = gw.session?.role
  if (role === 'admin' || role === 'manager') return <ManagerTerminal gw={gw} />

  return (
    <>
      <ClientTerminal gw={gw} />
      <button
        type="button"
        className="theme-toggle cap"
        onClick={() => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))}
        title="The dark palette is the same tokens redefined. The chart stays black in both."
      >
        {theme === 'dark' ? 'Light' : 'Dark'}
      </button>
    </>
  )
}
