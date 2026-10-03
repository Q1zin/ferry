<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, blocked, type JobRow, type NodeRow } from '../api'

const props = defineProps<{ node: NodeRow }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const jobs = ref<JobRow[]>([])
const loaded = ref(false)
const loading = ref(false)
const shown = ref('')
// null — лог ещё едет: не показывать под новым именем лог прошлой задачи.
const log = ref<string | null>(null)
const name = ref('')
const cmd = ref('')
const starting = ref(false)

const away = computed(() => props.node.paused || props.node.status?.online === false)
const off = computed(() => !!blocked(props.node))

async function load() {
  if (loading.value || off.value) return
  loading.value = true
  try {
    jobs.value = await api.jobs(props.node.name)
    loaded.value = true
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    loading.value = false
  }
}

// Лог какой задачи сейчас едет: тот же не запрашиваем дважды, другой — можно.
let fetching = ''
async function showLog(job: string) {
  if (shown.value !== job) {
    shown.value = job
    log.value = null
  }
  if (fetching === job) return
  fetching = job
  try {
    const text = await api.jobLog(props.node.name, job)
    // Пока шёл ssh, могли открыть лог другой задачи.
    if (shown.value === job) log.value = text
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    if (fetching === job) fetching = ''
  }
}

async function kill(job: string) {
  try {
    emit('say', await api.killJob(props.node.name, job))
    await load()
  } catch (e) {
    emit('say', String(e), true)
  }
}

async function start() {
  if (!name.value.trim() || !cmd.value.trim()) return
  starting.value = true
  try {
    emit('say', await api.startJob(props.node.name, name.value.trim(), cmd.value.trim()))
    const started = name.value.trim()
    name.value = ''
    cmd.value = ''
    await load()
    await showLog(started)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    starting.value = false
  }
}

// Список — как только узел ответил на опрос: открыть узел можно и раньше.
watch(
  off,
  (no) => {
    if (!no && !loaded.value) load()
  },
  { immediate: true },
)

// Пока открыт лог работающей задачи — дописываем его, и список задач освежаем.
let timer = 0
onMounted(() => {
  timer = window.setInterval(() => {
    load()
    const open = jobs.value.find((j) => j.name === shown.value)
    if (open?.running) showLog(open.name)
  }, 3000)
})
onUnmounted(() => clearInterval(timer))
</script>

<template>
  <div class="jobs">
    <p v-if="away" class="card muted">{{ blocked(node) }}</p>
    <template v-else>
      <div class="card">
        <h3>Новая задача</h3>
        <p class="muted small">
          Команда на рабочем столе узла, живёт после вызова: приложение, dev-сервер, сборка. То
          же имя ещё раз — перезапуск.
        </p>
        <div class="row">
          <input v-model="name" class="name" placeholder="имя: app" />
          <input v-model="cmd" class="grow" placeholder="команда" @keydown.enter="start" />
          <button
            class="primary"
            :disabled="starting || off || !name.trim() || !cmd.trim()"
            :aria-busy="starting"
            @click="start"
          >
            Запустить
          </button>
        </div>
      </div>

      <div class="card">
        <div class="row head">
          <h3>Задачи</h3>
          <button :disabled="loading || off" :aria-busy="loading" @click="load">Обновить</button>
        </div>
        <ul v-if="!loaded" class="list">
          <li v-for="i in 2" :key="i">
            <span class="dot probing"></span>
            <span class="info">
              <span class="skeleton short"></span>
              <span class="skeleton"></span>
            </span>
          </li>
        </ul>
        <p v-else-if="!jobs.length" class="muted">Задач нет.</p>
        <ul v-else class="list">
          <li v-for="job in jobs" :key="job.name" :class="{ selected: shown === job.name }">
            <span class="dot" :class="job.running ? 'ok' : 'off'"></span>
            <span class="info">
              <span>
                <strong>{{ job.name }}</strong>
                <span class="muted"> · {{ job.label }} · {{ job.ago }}</span>
              </span>
              <code>{{ job.cmd }}</code>
            </span>
            <span class="actions">
              <button v-if="job.running" class="danger" @click="kill(job.name)">Остановить</button>
              <button @click="showLog(job.name)">Лог</button>
            </span>
          </li>
        </ul>
      </div>

      <div v-if="shown" class="card">
        <div class="row head">
          <h3>Лог: {{ shown }}</h3>
          <button @click="shown = ''">Закрыть</button>
        </div>
        <pre v-if="log !== null" class="log">{{ log || '(пусто)' }}</pre>
        <div v-else class="log"><span class="skeleton"></span></div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.jobs {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.card {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.card h3,
.card p {
  margin: 0;
}

.small {
  font-size: 12px;
}

.head {
  justify-content: space-between;
}

.name {
  width: 140px;
}

.grow {
  flex: 1;
}

/* Список, а не таблица: команда — отдельной строкой, на узком окне не сжимается в столбик. */
.list {
  margin: 0;
  padding: 0;
  list-style: none;
}

.list li {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr) auto;
  gap: 10px;
  align-items: center;
  padding: 8px;
  border-bottom: 1px solid var(--line);
}

.list li:last-child {
  border-bottom: none;
}

.list li.selected {
  border-radius: 8px;
  background: color-mix(in srgb, var(--coral) 8%, transparent);
}

.info {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.info code {
  font: 12px ui-monospace, SFMono-Regular, Menlo, monospace;
  color: var(--muted);
  overflow-wrap: anywhere;
}

.skeleton.short {
  width: 8em;
}

.actions {
  display: flex;
  gap: 6px;
}

.log {
  min-height: 120px;
  max-height: 360px;
  overflow: auto;
  background: var(--paper);
  border-radius: 8px;
  padding: 10px;
}
</style>
