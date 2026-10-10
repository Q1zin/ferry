// Команды бэкенда (src-tauri/src/lib.rs) — типизированно. Состояние не опрашиваем: агент
// присылает его событием `state` при каждом изменении.
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

export type Mode = 'active' | 'no-desktop' | 'paused'
export type Role = 'both' | 'node' | 'console'

export type Status = {
  name: string
  role: Role
  mode: Mode
  os: string
  version: string
}

export type PeerView = {
  id: string
  name: string
  online: boolean
  status: Status | null
  direct: boolean | null
  rtt_ms: number | null
}

export type LanDevice = { id: string; name: string; pairing: boolean }

export type PairingInfo = { code: string; ticket: string; seconds_left: number }

export type Snapshot = {
  id: string
  me: Status
  peers: PeerView[]
  lan: LanDevice[]
  pairing: PairingInfo | null
}

export const api = {
  state: () => invoke<Snapshot>('state'),
  openPairing: () => invoke<PairingInfo>('open_pairing'),
  closePairing: () => invoke<void>('close_pairing'),
  pair: (code: string) => invoke<PeerView>('pair', { code }),
  setRole: (id: string | null, role: Role) => invoke<void>('set_role', { id, role }),
  rename: (id: string | null, name: string) => invoke<void>('rename', { id, name }),
  setMode: (mode: Mode) => invoke<void>('set_mode', { mode }),
  forget: (id: string) => invoke<void>('forget', { id }),
  autostart: () => invoke<boolean>('autostart'),
  setAutostart: (on: boolean) => invoke<void>('set_autostart', { on }),
  version: () => invoke<string>('version'),
}

export const onState = (cb: (s: Snapshot) => void): Promise<UnlistenFn> =>
  listen<Snapshot>('state', (e) => cb(e.payload))

export const onOpenPairing = (cb: () => void): Promise<UnlistenFn> => listen('open-pairing', cb)

export const modes: [Mode, string][] = [
  ['active', 'работает'],
  ['no-desktop', 'не трогать рабочий стол'],
  ['paused', 'отключён'],
]

export const roles: [Role, string][] = [
  ['both', 'узел и пульт'],
  ['node', 'узел'],
  ['console', 'пульт'],
]

export const modeLabel = (m: Mode) => modes.find(([k]) => k === m)?.[1] ?? m
export const roleLabel = (r: Role) => roles.find(([k]) => k === r)?.[1] ?? r

const osNames: Record<string, string> = { macos: 'macOS', windows: 'Windows', linux: 'Linux' }
export const osLabel = (os: string) => osNames[os] ?? os

/// Состояние устройства одним словом — для точки-индикатора.
export type Tone = 'ok' | 'quiet' | 'paused' | 'off' | 'warn'

export function tone(status: Status | null, online: boolean, version: string): Tone {
  if (!online || !status) return 'off'
  if (status.mode === 'paused') return 'paused'
  if (status.mode === 'no-desktop') return 'quiet'
  if (status.version !== version) return 'warn'
  return 'ok'
}

/// Путь до устройства: напрямую или через relay и задержка.
export function path(p: PeerView): string {
  if (!p.online) return 'не на связи'
  const how = p.direct === false ? 'через relay' : 'напрямую'
  return p.rtt_ms != null ? `${how}, ${p.rtt_ms} мс` : how
}
