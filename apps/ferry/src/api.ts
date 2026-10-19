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

/// Устройство на странице: это (`id` пуст) или связанное.
export type DeviceRef = { id: string | null; name: string; online: boolean; status: Status | null }

export type Detail = { properties: [string, string][]; warnings: string[] }

export type JobRow = { name: string; label: string; ago: string; running: boolean; cmd: string }

/// Что связанное устройство делало на этом: `at` — секунды Unix, `desk` — трогало рабочий стол.
export type Activity = { at: number; who: string; what: string; desk: boolean }

/// Доступы macOS: запись экрана и «Универсальный доступ» (ввод). На других системах — null.
export type Permissions = { screen: boolean; input: boolean }

export type Snapshot = {
  id: string
  me: Status
  peers: PeerView[]
  lan: LanDevice[]
  pairing: PairingInfo | null
  activity: Activity[]
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
  detail: (id: string) => invoke<Detail>('detail', { id }),
  screenshot: (id: string) => invoke<string>('screenshot', { id }),
  input: (id: string, args: string[]) => invoke<string>('input', { id, args }),
  jobs: (id: string) => invoke<JobRow[]>('jobs', { id }),
  jobLog: (id: string, name: string) => invoke<string>('job_log', { id, name }),
  startJob: (id: string, name: string, cmd: string) => invoke<string>('start_job', { id, name, cmd }),
  killJob: (id: string, name: string) => invoke<string>('kill_job', { id, name }),
  power: (id: string, action: string) => invoke<string>('power', { id, action }),
  cli: () => invoke<{ path: string | null; installed: string | null }>('cli'),
  installCli: () => invoke<string>('install_cli'),
  /// Версия новее этой, если есть в релизах.
  checkUpdate: () => invoke<string | null>('check_update'),
  installUpdate: () => invoke<string>('install_update'),
  updateAll: () => invoke<string>('update_all'),
  permissions: () => invoke<Permissions | null>('permissions'),
  askPermission: (kind: 'screen' | 'input') => invoke<void>('ask_permission', { kind }),
  restartApp: () => invoke<void>('restart_app'),
}

/// Почему с устройством сейчас нельзя работать; пусто — можно. `desk` — нужен ещё и рабочий
/// стол. Те же правила, что проверяет само устройство.
export function blocked(d: DeviceRef, desk = false): string {
  const s = d.status
  if (!d.online || !s) return 'Устройство не на связи.'
  if (s.mode === 'paused') return 'Человек за устройством отключил его от других — пока режим не вернут, команды не пройдут.'
  if (s.role === 'console') return 'Это пульт: команды не принимает. Роль меняется во вкладке «Обзор».'
  if (desk && s.mode === 'no-desktop') {
    return 'На устройстве режим «не трогать рабочий стол»: экран и ввод недоступны.'
  }
  return ''
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
