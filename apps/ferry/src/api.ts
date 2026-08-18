// Команды бэкенда (src-tauri/src/lib.rs) — типизированно. Каждая ходит в узел по ssh,
// поэтому всё асинхронно, а ошибки — строки от ferry.
import { invoke } from '@tauri-apps/api/core'

export type NodeEntry = {
  name: string
  addr: string
  user: string
  paused: boolean
}

export type Status = {
  online: boolean
  summary: string
  mode: string | null
  desk: boolean
  latency_ms: number | null
}

/// Узел из реестра и то, что о нём уже известно: `null` — ответ ещё не пришёл.
export type NodeRow = NodeEntry & { status: Status | null; warnings: string[] | null }

export type Detail = {
  properties: [string, string][]
  warnings: string[]
}

export type JobRow = {
  name: string
  label: string
  ago: string
  running: boolean
  cmd: string
}

export const api = {
  nodes: () => invoke<NodeEntry[]>('nodes'),
  status: (host: string) => invoke<Status>('status', { host }),
  detail: (host: string) => invoke<Detail>('detail', { host }),
  jobs: (host: string) => invoke<JobRow[]>('jobs', { host }),
  jobLog: (host: string, name: string) => invoke<string>('job_log', { host, name }),
  startJob: (host: string, name: string, cmd: string) =>
    invoke<string>('start_job', { host, name, cmd }),
  killJob: (host: string, name: string) => invoke<string>('kill_job', { host, name }),
  screenshot: (host: string) => invoke<string>('screenshot', { host }),
  input: (host: string, args: string[]) => invoke<string>('input', { host, args }),
  addNode: (name: string, target: string) => invoke<void>('add_node', { name, target }),
  pause: (host: string, paused: boolean) => invoke<void>('pause', { host, paused }),
  rename: (host: string, newName: string) => invoke<void>('rename', { host, newName }),
  remove: (host: string) => invoke<void>('remove', { host }),
  find: (host: string) => invoke<string | null>('find', { host }),
  restart: (host: string) => invoke<string>('restart', { host }),
  power: (host: string, action: string) => invoke<string>('power', { host, action }),
  trusted: () => invoke<string[]>('trusted'),
  trust: (host: string) => invoke<void>('trust', { host }),
  version: () => invoke<string>('version'),
}

/// Состояние узла одним словом — для точки-индикатора и подписи.
export type Tone = 'ok' | 'quiet' | 'paused' | 'off' | 'warn' | 'probing'

export function tone(node: NodeRow): Tone {
  const s = node.status
  if (node.paused) return 'paused'
  if (!s) return 'probing'
  if (!s.online) return 'off'
  if (s.mode === 'отключён от хаба') return 'paused'
  if (s.mode === 'не трогать рабочий стол') return 'quiet'
  if (!s.desk || node.warnings?.length) return 'warn'
  return 'ok'
}

/// Почему с узлом сейчас нельзя работать; пусто — можно. `desk` — нужен ещё и рабочий стол.
export function blocked(node: NodeRow, desk = false): string {
  const s = node.status
  if (node.paused) return 'Узел на паузе на хабе.'
  if (!s) return 'Опрашиваю узел…'
  if (!s.online) return 'Узел не на связи. Если он сменил адрес — «Найти» в «Обзоре».'
  if (desk && s.mode && s.mode !== 'работает') {
    return `На узле режим «${s.mode}» — рабочий стол трогать нельзя.`
  }
  return ''
}
