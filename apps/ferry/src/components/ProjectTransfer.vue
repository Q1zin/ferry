<script setup lang="ts">
import { ref } from 'vue'
import { api, type ProjectFiles, type ProjectReceipt, type ProjectResult } from '../api'
import { formatBytes } from '../projectDisplay'

const props = defineProps<{ receipt: ProjectReceipt; peer: string }>()
const labels: Record<ProjectReceipt['state'], string> = {
  preparing: 'Подготовка',
  transferring: 'Передача',
  ready: 'Скопировано',
  starting: 'Запуск команды',
  started: 'Команда запущена',
  failed: 'Ошибка',
  interrupted: 'Прервано',
}
const jobStates: Record<string, string> = {
  running: 'В работе',
  queued: 'В очереди',
  succeeded: 'Успешно',
  finished: 'Завершено',
  failed: 'Ошибка',
  cancelled: 'Отменено',
  interrupted: 'Прервано',
  unavailable: 'Нет данных',
}
const date = new Intl.DateTimeFormat(undefined, { dateStyle: 'short', timeStyle: 'short' })
const files = ref<ProjectFiles | null>(null)
const folders = ref<string[]>([])
const fileError = ref('')
const browsing = ref(false)
const result = ref<ProjectResult | null>(null)
const resultError = ref('')
const loadingResult = ref(false)

async function browse(parts: string[]) {
  const { source, direction, peer_id } = props.receipt
  if (!source || browsing.value) return
  browsing.value = true
  fileError.value = ''
  try {
    files.value = await api.projectFiles(direction === 'incoming' ? null : peer_id, source, parts[parts.length - 1] ?? '')
    folders.value = parts
  } catch (e) {
    fileError.value = String(e)
  } finally {
    browsing.value = false
  }
}

async function loadResult() {
  const { id, direction, run } = props.receipt
  if (!run || loadingResult.value) return
  loadingResult.value = true
  resultError.value = ''
  result.value = null
  try {
    const reply = await api.projectTransfer('result', { id, direction })
    if (reply.run_id !== run.run_id) throw new Error('Получен результат другого запуска.')
    result.value = reply
  } catch (e) {
    resultError.value = String(e)
  } finally {
    loadingResult.value = false
  }
}
</script>

<template>
  <details class="card transfer">
    <summary>
      <strong>{{ receipt.project }}</strong>
      <span class="muted"> · {{ receipt.direction === 'incoming' ? 'от' : 'на' }} {{ peer }}</span>
      <span :class="{ warning: ['failed', 'interrupted'].includes(receipt.state) }"> · {{ labels[receipt.state] }}</span>
    </summary>
    <div class="transfer-body">
      <div class="muted">Обновлено {{ date.format(receipt.updated * 1000) }}</div>
      <div v-if="receipt.cwd">Папка на устройстве <code>{{ receipt.cwd }}</code></div>
      <div v-else-if="receipt.source">Рабочая копия <code>{{ receipt.source.workspace }}</code></div>
      <div v-if="receipt.source">Снимок <code :title="receipt.source.commit">{{ receipt.source.commit.slice(0, 12) }}</code></div>
      <p v-if="receipt.error" class="warning" role="alert">{{ receipt.error }}</p>

      <section v-if="receipt.run" class="transfer-detail">
        <h3>Команда</h3>
        <div class="row">
          <code>{{ receipt.run.name }}</code>
          <span class="muted">{{ jobStates[result?.state ?? receipt.run.state] ?? result?.state ?? receipt.run.state }}</span>
          <span v-if="result?.exit_code != null">Код выхода {{ result.exit_code }}</span>
          <button :disabled="loadingResult" @click="loadResult">{{ result ? 'Обновить лог' : 'Посмотреть результат' }}</button>
        </div>
        <p v-if="resultError" class="warning" role="alert">{{ resultError }}</p>
        <pre v-if="result?.log">{{ result.log }}</pre>
        <p v-else-if="result" class="muted">В логе пока пусто.</p>
      </section>

      <section v-if="receipt.source && ['ready', 'starting', 'started', 'failed', 'interrupted'].includes(receipt.state)" class="transfer-detail">
        <div class="row">
          <button :disabled="browsing" @click="browse(folders)">{{ files ? 'Обновить файлы' : 'Файлы рабочей копии' }}</button>
          <button v-if="folders.length" :disabled="browsing" @click="browse(folders.slice(0, -1))">На уровень выше</button>
          <code v-if="files?.directory">{{ files.directory }}</code>
        </div>
        <p v-if="fileError" class="warning" role="alert">{{ fileError }}</p>
        <p v-if="files && !files.entries.length" class="muted">Папка пуста.</p>
        <div v-if="files?.entries.length" class="project-file-list">
          <ul aria-label="Файлы проекта">
            <li v-for="entry in files.entries" :key="entry.path" class="row">
              <button v-if="entry.directory" :disabled="browsing" @click="browse([...folders, entry.path])">{{ entry.name }}</button>
              <code v-else>{{ entry.name }}</code>
              <span class="muted">
                {{ entry.directory ? 'папка · ' : '' }}{{ entry.bytes == null ? 'размер не определён' : formatBytes(entry.bytes) }}
              </span>
            </li>
          </ul>
        </div>
        <p v-if="files?.truncated" class="muted">Список файлов или размеры показаны частично.</p>
      </section>
    </div>
  </details>
</template>

<style scoped>
.transfer { min-width: 0; overflow-wrap: anywhere; }
.transfer summary { cursor: pointer; }
.transfer-body, .transfer-detail { display: grid; gap: 8px; min-width: 0; }
.transfer-body { margin-top: 10px; }
.transfer-detail { padding-top: 8px; border-top: 1px solid var(--line); }
.transfer-body p, .project-file-list ul { margin: 0; }
.project-file-list ul { list-style: none; padding: 0; display: grid; gap: 4px; }
.project-file-list { max-height: 240px; overflow: auto; }
.transfer pre { max-height: 240px; overflow: auto; }
</style>
