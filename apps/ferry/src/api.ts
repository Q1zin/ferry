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
export type HumanQuestion = { id: string; text: string; from: string; expires_at: number; state: string }

export type Snapshot = {
  id: string
  me: Status
  peers: PeerView[]
  lan: LanDevice[]
  pairing: PairingInfo | null
  activity: Activity[]
}

export const desktop = '__TAURI_INTERNALS__' in window
let token = ''
if (!desktop) {
  const fragment = new URLSearchParams(location.hash.slice(1))
  token = fragment.get('token') ?? sessionStorage.getItem('ferry-token') ?? ''
  if (fragment.has('token')) {
    sessionStorage.setItem('ferry-token', token)
    history.replaceState(null, '', location.pathname)
  }
}
export type DashboardRequest = { action: string; id?: string; value?: string; cursor?: string }
export async function dashboard<T>(request: DashboardRequest): Promise<T> {
  if (desktop) return invoke<T>('dashboard', { request })
  const reply = await fetch('/api', {
    method: 'POST', headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` }, body: JSON.stringify(request),
  })
  const value = await reply.json()
  if (!reply.ok) throw new Error(value.error ?? `HTTP ${reply.status}`)
  return value as T
}
export async function downloadReport(id: string): Promise<ExportReport> {
  const result = await dashboard<ExportReport>({ action: 'export', id })
  if (result.download_id) {
    const reply = await fetch(`/download/${encodeURIComponent(result.download_id)}`, { headers: { authorization: `Bearer ${token}` } })
    if (!reply.ok) throw new Error('Не удалось скачать архив; повтори экспорт')
    const url = URL.createObjectURL(await reply.blob())
    const link = document.createElement('a'); link.href = url; link.download = result.filename ?? 'ferry-report.zip'; link.click()
    window.setTimeout(() => URL.revokeObjectURL(url), 1000)
  }
  return result
}
export type ExportReport = { complete: boolean; errors: string[]; archive_file?: string; directory?: string; download_id?: string; filename?: string }
export type TestSummary = { tests: number; passed: number; failed: number; errors: number; skipped: number; artifacts?: number; warnings?: string[] }
export type RunSummary = { id: string; profile: string; created: number; finished_at: number | null; duration_seconds: number | null; state: string; worker_active: boolean; total: number; succeeded: number; devices: {device: string; state: string}[]; summary: TestSummary }
export type RunList = { checks: RunSummary[]; next_cursor: string | null; errors: string[]; scan_truncated?: boolean; legacy_count?: number }
export type RunPhase = { name: string; job: string; run_id: string | null; started: number; finished_seen?: number | null; result: Record<string, unknown> | null }
export type RunTarget = { device: string; state: string; duration_seconds?: number | null; phases: RunPhase[]; tests?: TestSummary; test_cases?: {count?: number; complete?: boolean; omitted_cases?: number; error?: string; preview_error?: string; preview?: {cases:{suite:string[];class:string;name:string;state:string;duration_seconds?:number;message?:string;detail?:string}[];more:number;complete:boolean}}; artifacts?: {id: string; name?: string; path?: string; bytes?: number}[]; errors?: string[]; cleanup_pending?: boolean; source?: unknown }
export type RunReport = { id: string; profile: string; state: string; created?: number; duration_seconds?: number | null; worker_active?: boolean; results: RunTarget[] }
export type SetupStatus = { agents: { id: 'claude' | 'codex'; available: boolean; skill_installed: boolean; mcp_configured: boolean; command: string | null; config_conflict?: boolean; error?: string | null }[]; cli_available: boolean; app_available: boolean }

export const api = {
  state: () => desktop ? invoke<Snapshot>('state') : dashboard<Snapshot>({ action: 'snapshot' }),
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
  questions: () => invoke<HumanQuestion[]>('questions'),
  answerQuestion: (id: string, answer: 'done' | 'declined') => invoke<void>('answer', { id, answer }),
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
  desktop ? listen<Snapshot>('state', (e) => cb(e.payload)) : Promise.resolve(() => {})

export const onOpenPairing = (cb: () => void): Promise<UnlistenFn> => desktop ? listen('open-pairing', cb) : Promise.resolve(() => {})
export const onHumanQuestions = (cb: (q: HumanQuestion[]) => void): Promise<UnlistenFn> =>
  listen<HumanQuestion[]>('human-questions', (e) => cb(e.payload))

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
