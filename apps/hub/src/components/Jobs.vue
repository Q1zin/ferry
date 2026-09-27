<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { api, type JobRow, type NodeRow } from '../api'

const props = defineProps<{ node: NodeRow }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const jobs = ref<JobRow[]>([])
const loading = ref(false)
const shown = ref('')
const log = ref('')
const name = ref('')
const cmd = ref('')
const starting = ref(false)

async function load() {
  if (loading.value || props.node.paused || !props.node.online) return
  loading.value = true
  try {
    jobs.value = await api.jobs(props.node.name)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    loading.value = false
  }
}

async function showLog(job: string) {
  shown.value = job
  try {
    log.value = await api.jobLog(props.node.name, job)
  } catch (e) {
    emit('say', String(e), true)
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

// Пока открыт лог работающей задачи — дописываем его, и список задач освежаем.
let timer = 0
onMounted(() => {
  load()
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
    <p v-if="node.paused || !node.online" class="card muted">
      {{ node.paused ? 'Узел на паузе на хабе.' : 'Узел не на связи.' }}
    </p>
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
          <button class="primary" :disabled="starting || !name.trim() || !cmd.trim()" @click="start">
            {{ starting ? 'Запускаю…' : 'Запустить' }}
          </button>
        </div>
      </div>

      <div class="card">
        <div class="row head">
          <h3>Задачи</h3>
          <button :disabled="loading" @click="load">{{ loading ? '…' : 'Обновить' }}</button>
        </div>
        <p v-if="!jobs.length" class="muted">Задач нет.</p>
        <table v-else>
          <tr v-for="job in jobs" :key="job.name" :class="{ selected: shown === job.name }">
            <td>
              <span class="jname">
                <span class="dot" :class="job.running ? 'ok' : 'off'"></span>
                {{ job.name }}
              </span>
            </td>
            <td>{{ job.label }}</td>
            <td class="muted">{{ job.ago }}</td>
            <td class="cmd"><code>{{ job.cmd }}</code></td>
            <td class="actions">
              <button @click="showLog(job.name)">Лог</button>
              <button v-if="job.running" class="danger" @click="kill(job.name)">Остановить</button>
            </td>
          </tr>
        </table>
      </div>

      <div v-if="shown" class="card">
        <div class="row head">
          <h3>Лог: {{ shown }}</h3>
          <button @click="shown = ''">Закрыть</button>
        </div>
        <pre class="log">{{ log || '(пусто)' }}</pre>
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

table {
  border-collapse: collapse;
  width: 100%;
}

td {
  padding: 6px 8px 6px 0;
  border-bottom: 1px solid var(--line);
  vertical-align: middle;
}

tr:last-child td {
  border-bottom: none;
}

tr.selected td {
  background: color-mix(in srgb, var(--coral) 8%, transparent);
}

.jname {
  display: inline-flex;
  gap: 8px;
  align-items: center;
  white-space: nowrap;
}

.cmd code {
  font: 12px ui-monospace, SFMono-Regular, Menlo, monospace;
  word-break: break-all;
}

.actions {
  white-space: nowrap;
  text-align: right;
}

.log {
  max-height: 360px;
  overflow: auto;
  background: var(--paper);
  border-radius: 8px;
  padding: 10px;
}
</style>
