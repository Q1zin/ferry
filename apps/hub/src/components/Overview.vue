<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, blocked, type Detail, type NodeRow } from '../api'

const props = defineProps<{ node: NodeRow }>()
const emit = defineEmits<{
  say: [text: string, error?: boolean]
  changed: []
  renamed: [name: string]
  gone: []
}>()

const detail = ref<Detail | null>(null)
const loading = ref(false)
const busy = ref('')
const newName = ref('')

// Узел на паузе или не на связи — показываем почему; пока опрашивается — заглушку.
const away = computed(() => props.node.paused || props.node.status?.online === false)
const off = computed(() => !!blocked(props.node))
const mode = computed(() => {
  const s = props.node.status
  if (props.node.paused || s?.online === false) return '—'
  return s ? (s.mode ?? 'работает') : null
})

async function load() {
  if (off.value || loading.value) return
  loading.value = true
  try {
    detail.value = await api.detail(props.node.name)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    loading.value = false
  }
}

/// Действие с узлом: кнопка занята, пока идёт; ответ ferry — в уведомление.
async function act(name: string, run: () => Promise<unknown>, done?: string) {
  busy.value = name
  try {
    const said = await run()
    emit('say', typeof said === 'string' && said.trim() ? said : (done ?? 'Готово'))
    emit('changed')
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = ''
  }
}

function togglePause() {
  const paused = !props.node.paused
  act('pause', () => api.pause(props.node.name, paused), paused ? 'На паузе' : 'Снова в работе')
}

function restart() {
  act('restart', () => api.restart(props.node.name))
}

function cancelPower() {
  act('cancel', () => api.power(props.node.name, 'cancel'))
}

function power(action: string, question: string) {
  if (!confirm(question)) return
  act(action, () => api.power(props.node.name, action))
}

async function rename() {
  const name = newName.value.trim()
  if (!name || name === props.node.name) return
  busy.value = 'rename'
  try {
    await api.rename(props.node.name, name)
    emit('say', `${props.node.name} теперь ${name}`)
    emit('renamed', name)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = ''
  }
}

async function remove() {
  if (!confirm(`Убрать ${props.node.name} с хаба? ferry на самом узле останется.`)) return
  try {
    await api.remove(props.node.name)
    emit('say', `${props.node.name} убран с хаба`)
    emit('gone')
  } catch (e) {
    emit('say', String(e), true)
  }
}

async function find() {
  busy.value = 'find'
  try {
    const addr = await api.find(props.node.name)
    emit('say', addr ? `${props.node.name}: ${addr}` : `${props.node.name} в сети не нашёлся`, !addr)
    emit('changed')
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = ''
  }
}

// Свойства — как только узел ответил на опрос: открыть узел можно и раньше.
watch(
  off,
  (no) => {
    if (!no && !detail.value) load()
  },
  { immediate: true },
)
</script>

<template>
  <div class="overview">
    <div class="card">
      <div class="row head">
        <h3>Свойства</h3>
        <button :disabled="loading || off" :aria-busy="loading" @click="load">Обновить</button>
      </div>
      <p v-if="away" class="muted">{{ blocked(node) }}</p>
      <template v-else-if="detail">
        <table>
          <tr v-for="[label, value] in detail.properties" :key="label">
            <th>{{ label }}</th>
            <td>{{ value }}</td>
          </tr>
        </table>
        <ul v-if="detail.warnings.length" class="warnings">
          <li v-for="w in detail.warnings" :key="w" class="warning">⚠ {{ w }}</li>
        </ul>
      </template>
      <!-- Столько же строк, сколько отдаёт узел: таблица встаёт на место заглушки. -->
      <table v-else>
        <tr v-for="i in 12" :key="i">
          <th><span class="skeleton short"></span></th>
          <td><span class="skeleton"></span></td>
        </tr>
      </table>
    </div>

    <div class="side">
      <div class="card">
        <h3>Режим узла</h3>
        <p>
          <template v-if="mode">{{ mode }}</template>
          <span v-else class="skeleton short"></span>
        </p>
        <p class="muted small">
          Режим выбирает человек за узлом — иконкой ferry в трее: работать, не трогать рабочий
          стол, отключиться от хаба.
        </p>
      </div>

      <div class="card">
        <h3>Узел на хабе</h3>
        <div class="row">
          <input v-model="newName" class="grow" :placeholder="node.name" @keydown.enter="rename" />
          <button
            :disabled="!!busy || !newName.trim()"
            :aria-busy="busy === 'rename'"
            @click="rename"
          >
            Переименовать
          </button>
        </div>
        <div class="row">
          <button :disabled="!!busy" :aria-busy="busy === 'pause'" @click="togglePause">
            {{ node.paused ? 'Вернуть в работу' : 'Пауза на хабе' }}
          </button>
          <button :disabled="!!busy || node.paused" :aria-busy="busy === 'find'" @click="find">
            Найти
          </button>
          <button class="danger" :disabled="!!busy" @click="remove">Убрать</button>
        </div>
      </div>

      <div class="card">
        <h3>Обслуживание</h3>
        <div class="row">
          <button :disabled="!!busy || off" :aria-busy="busy === 'restart'" @click="restart">
            Перезапустить демон
          </button>
        </div>
        <div class="row">
          <button
            :disabled="!!busy || off"
            :aria-busy="busy === 'reboot'"
            @click="power('reboot', `Перезагрузить ${node.name}? Через 10 с, можно отменить.`)"
          >
            Перезагрузить
          </button>
          <button
            :disabled="!!busy || off"
            :aria-busy="busy === 'sleep'"
            @click="power('sleep', `Усыпить ${node.name}? Разбудить по сети получится только по кабелю.`)"
          >
            Усыпить
          </button>
          <button
            class="danger"
            :disabled="!!busy || off"
            :aria-busy="busy === 'shutdown'"
            @click="power('shutdown', `Выключить ${node.name}? Через 10 с, можно отменить.`)"
          >
            Выключить
          </button>
          <button :disabled="!!busy || off" :aria-busy="busy === 'cancel'" @click="cancelPower">
            Отменить
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.overview {
  display: grid;
  grid-template-columns: minmax(0, 1.4fr) minmax(280px, 1fr);
  gap: 16px;
  align-items: start;
}

/* Узкое окно: действия — под свойствами, а не впритык сбоку. */
@media (max-width: 1000px) {
  .overview {
    grid-template-columns: minmax(0, 1fr);
  }
}

.head {
  justify-content: space-between;
  margin-bottom: 10px;
}

table {
  border-collapse: collapse;
  width: 100%;
}

th,
td {
  text-align: left;
  vertical-align: top;
  padding: 5px 0;
  border-bottom: 1px solid var(--line);
}

th {
  width: 40%;
  font-weight: normal;
  color: var(--muted);
  padding-right: 12px;
}

tr:last-child th,
tr:last-child td {
  border-bottom: none;
}

.warnings {
  margin: 12px 0 0;
  padding: 0;
  list-style: none;
}

.side {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.side .card {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.side p {
  margin: 0;
}

.small {
  font-size: 12px;
}

.grow {
  flex: 1;
}

.skeleton.short {
  width: 6em;
}
</style>
