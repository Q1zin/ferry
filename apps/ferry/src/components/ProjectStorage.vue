<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, blocked, supportsProjectFeature, type PeerView, type StorageKind, type StorageReport, type StorageRequest } from '../api'
import { formatBytes } from '../projectDisplay'

const props = defineProps<{ peers: PeerView[] }>()
const kinds: Record<StorageKind, string> = {
  projects: 'Проекты', workspaces: 'Рабочие копии', caches: 'Кеши', jobs: 'Задачи', archives: 'Архивы задач',
  outputs: 'Результаты', runners: 'Надсмотрщики задач', checks: 'Проверки', recordings: 'Записи экрана',
  shots: 'Снимки экрана', observations: 'Наблюдения', diagnostics: 'Диагностика', questions: 'Запросы человеку',
}
const readOnly = new Set<StorageKind>(['projects', 'workspaces', 'jobs', 'archives'])
const device = ref('')
const kind = ref<StorageKind | ''>('')
const days = ref<number | ''>(7)
const report = ref<StorageReport | null>(null)
const preview = ref<{ device: string | null; request: StorageRequest; report: StorageReport } | null>(null)
const cleanup = ref<StorageReport | null>(null)
const busy = ref(false)
const error = ref('')
const limit = ref(40)

function reason(peer: PeerView) {
  return blocked(peer) || (supportsProjectFeature(peer.status, 23) ? '' : 'Нужна Ferry 0.1.23 или новее.')
}

const deviceId = computed(() => device.value || null)
const selectedPeer = computed(() => props.peers.find(peer => peer.id === device.value))
const deviceError = computed(() => device.value
  ? selectedPeer.value ? reason(selectedPeer.value) : 'Устройство больше не связано.'
  : '')
const validDays = computed(() => typeof days.value === 'number' && Number.isInteger(days.value) && days.value >= 0 && days.value <= 36500)
const categories = computed(() => (Object.keys(kinds) as StorageKind[]).filter(key => report.value?.categories[key] !== undefined))
const toFree = computed(() => preview.value?.report.candidates.reduce((total, entry) => total + entry.bytes, 0) ?? 0)

async function scan() {
  if (busy.value || deviceError.value) return
  busy.value = true
  error.value = ''
  preview.value = null
  try {
    report.value = await api.projectStorage(deviceId.value, kind.value ? { kind: kind.value } : {})
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = false
  }
}

async function plan(itemKind: StorageKind, only?: string) {
  if (busy.value || deviceError.value || readOnly.has(itemKind) || (!only && !validDays.value)) return
  const target = deviceId.value
  const request: StorageRequest = { kind: itemKind, ...(only ? { only } : {}), older_than_days: only ? 0 : Number(days.value) }
  busy.value = true
  error.value = ''
  preview.value = null
  cleanup.value = null
  try {
    preview.value = { device: target, request, report: await api.projectStorage(target, request) }
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = false
  }
}

async function apply() {
  const confirmed = preview.value
  if (busy.value || deviceError.value || !confirmed?.report.candidates.length || confirmed.device !== deviceId.value) return
  preview.value = null
  busy.value = true
  error.value = ''
  try {
    cleanup.value = await api.projectStorage(confirmed.device, { ...confirmed.request, apply: confirmed.report.preview_token })
    report.value = await api.projectStorage(confirmed.device, kind.value ? { kind: kind.value } : {})
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = false
  }
}

watch([device, kind], () => {
  report.value = null
  cleanup.value = null
  preview.value = null
  error.value = ''
  limit.value = 40
  void scan()
}, { immediate: true })
watch(days, () => { preview.value = null })
</script>

<template>
  <section class="card project-storage" :aria-busy="busy">
    <h2>Место на устройствах</h2>
    <div class="row storage-options">
      <label>Устройство
        <select v-model="device" aria-label="Устройство" :disabled="busy">
          <option value="">Это устройство</option>
          <option v-for="peer in peers" :key="peer.id" :value="peer.id" :disabled="!!reason(peer)">{{ peer.name }}</option>
        </select>
      </label>
      <label>Категория
        <select v-model="kind" :disabled="busy">
          <option value="">Все категории</option>
          <option v-for="(name, key) in kinds" :key="key" :value="key">{{ name }}</option>
        </select>
      </label>
      <button :disabled="busy || !!deviceError" @click="scan">Обновить размеры</button>
    </div>
    <p v-if="deviceError" class="warning" role="alert">{{ deviceError }}</p>
    <p v-if="error" class="warning" role="alert">{{ error }}</p>
    <p v-if="busy" class="muted" role="status">Проверяю файлы устройства…</p>
    <p v-if="cleanup" role="status">Освобождено {{ formatBytes(cleanup.freed_bytes) }}</p>
    <ul v-if="cleanup?.errors.length" class="warning">
      <li v-for="failure in cleanup.errors" :key="failure.path"><code>{{ failure.path }}</code> · {{ failure.error }}</li>
    </ul>

    <template v-if="report">
      <p><strong>{{ formatBytes(report.logical_bytes) }}</strong> занимают файлы Ferry и рабочие копии</p>
      <ul v-if="!kind && categories.length" class="storage-categories">
        <li v-for="category in categories" :key="category">
          <button :disabled="busy" @click="kind = category">{{ kinds[category] }}</button>
          <span class="muted">{{ formatBytes(report.categories[category] ?? 0) }}</span>
        </li>
      </ul>
      <div v-if="kind && !readOnly.has(kind) && report.entries.length" class="row storage-options">
        <label>Возраст файлов, дней
          <input v-model.number="days" type="number" min="0" max="36500" :disabled="busy" />
        </label>
        <button :disabled="busy || !!deviceError || !validDays" @click="plan(kind)">Проверить, что можно очистить</button>
      </div>
    </template>

    <div v-if="preview" class="cleanup-plan" aria-label="Предпросмотр очистки">
      <h3>Предпросмотр очистки</h3>
      <p v-if="!preview.report.candidates.length" class="muted">Нет подходящих файлов для очистки.</p>
      <template v-else>
        <p>{{ preview.report.candidates.length }} позиций · {{ formatBytes(toFree) }}</p>
        <p v-if="preview.request.kind === 'caches'" class="muted">Перед удалением кеша останови сборки, запущенные вне Ferry.</p>
        <ul class="cleanup-candidates">
          <li v-for="candidate in preview.report.candidates" :key="candidate.id"><code>{{ candidate.path }}</code> · {{ formatBytes(candidate.bytes) }}</li>
        </ul>
        <p v-if="preview.report.more_candidates" class="muted">В эту очистку войдут только перечисленные позиции.</p>
      </template>
      <div class="row">
        <button v-if="preview.report.candidates.length" class="danger" :disabled="busy || !!deviceError" @click="apply">Удалить выбранные файлы</button>
        <button :disabled="busy" @click="preview = null">Закрыть предпросмотр</button>
      </div>
    </div>

    <template v-if="report">
      <p v-if="!report.entries.length && !busy" class="muted">Файлов в этой категории нет.</p>
      <ul v-else-if="report.entries.length" class="storage-entries">
        <li v-for="entry in report.entries.slice(0, limit)" :key="entry.id">
          <code>{{ entry.path }}</code>
          <div class="row">
            <span class="muted">{{ kinds[entry.kind] }} · {{ formatBytes(entry.bytes) }} · файлов {{ entry.files }}</span>
            <button v-if="!entry.protected && !readOnly.has(entry.kind)" :disabled="busy || !!deviceError" @click="plan(entry.kind, entry.id)">Проверить очистку</button>
          </div>
          <p v-if="entry.protected" class="muted">{{ entry.protected }}</p>
        </li>
      </ul>
      <button v-if="report.entries.length > limit" class="more-files" @click="limit += 40">Показать ещё файлы</button>
      <p v-if="report.entries_truncated" class="muted">Показана часть из {{ report.entry_count }} позиций. Выбери категорию, чтобы сузить список.</p>
    </template>
  </section>
</template>

<style scoped>
.project-storage, .cleanup-plan { display: grid; gap: 12px; min-width: 0; }
.project-storage { overflow-wrap: anywhere; }
.project-storage p, .project-storage ul { margin: 0; }
.project-storage ul { padding-left: 20px; }
.storage-options { align-items: end; }
.storage-options label { display: grid; gap: 5px; min-width: 0; }
.storage-options input { width: 104px; }
ul.storage-categories, ul.storage-entries { list-style: none; padding: 0; display: grid; gap: 8px; }
.storage-categories { grid-template-columns: repeat(auto-fit, minmax(215px, 1fr)); }
.storage-categories li { display: flex; align-items: center; gap: 8px; }
.storage-entries li { display: grid; gap: 4px; padding: 8px 0; border-top: 1px solid var(--line); }
.cleanup-plan { margin: 4px 0; padding: 12px; border: 1px solid var(--amber); border-radius: 10px; }
.cleanup-candidates { max-height: 200px; overflow: auto; }
.more-files { justify-self: start; }
</style>
