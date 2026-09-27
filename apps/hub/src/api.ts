// Команды бэкенда (src-tauri/src/lib.rs) — типизированно. Каждая ходит в узел по ssh,
// поэтому всё асинхронно, а ошибки — строки от ferry.
import { invoke } from '@tauri-apps/api/core'

export type NodeRow = {
  name: string
  addr: string
  user: string
  paused: boolean
  online: boolean
  summary: string
  mode: string | null
  desk: boolean
  latency_ms: number | null
  warnings: string[]
}

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
  nodes: () => invoke<NodeRow[]>('nodes'),
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
export type Tone = 'ok' | 'quiet' | 'paused' | 'off' | 'warn'

export function tone(node: NodeRow): Tone {
  if (node.paused) return 'paused'
  if (!node.online) return 'off'
  if (node.mode === 'отключён от хаба') return 'paused'
  if (node.mode === 'не трогать рабочий стол') return 'quiet'
  if (!node.desk || node.warnings.length) return 'warn'
  return 'ok'
}
