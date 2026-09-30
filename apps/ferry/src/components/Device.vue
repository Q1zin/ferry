<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import {
  api,
  blocked,
  modeLabel,
  modes,
  osLabel,
  roles,
  tone,
  type Activity,
  type Detail,
  type Mode,
  type Role,
  type Status,
} from '../api'
import Jobs from './Jobs.vue'
import Screen from './Screen.vue'

const props = defineProps<{
  device: { id: string | null; name: string; online: boolean; status: Status | null; path: string }
  version: string
  /// Журнал этого устройства: что делали связанные.
  activity?: Activity[]
}>()
const emit = defineEmits<{ say: [text: string, error?: boolean]; gone: [] }>()

const busy = ref('')
const newName = ref('')
const me = computed(() => props.device.id === null)
const s = computed(() => props.device.status)
// Чужое устройство слушается, пока на связи и человек за ним его не отключил.
const reachable = computed(() => me.value || (props.device.online && s.value?.mode !== 'paused'))

const tabs = [
  ['overview', 'Обзор'],
  ['screen', 'Экран'],
  ['jobs', 'Задачи'],
] as const
const tab = ref<(typeof tabs)[number][0]>('overview')

// Журнал: новые сверху.
const journal = computed(() => [...(props.activity ?? [])].reverse())
const clock = (at: number) => new Date(at * 1000).toLocaleTimeString('ru-RU')

// Свойства чужого устройства — как только оно на связи и принимает команды.
const detail = ref<Detail | null>(null)
const detailLoading = ref(false)
async function loadDetail() {
  if (me.value || detailLoading.value || blocked(props.device)) return
  detailLoading.value = true
  try {
    detail.value = await api.detail(props.device.id!)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    detailLoading.value = false
  }
}
watch(
  () => !blocked(props.device),
  (ok) => {
    if (ok && !detail.value) loadDetail()
  },
  { immediate: true },
)

function power(action: string) {
  act(`power:${action}`, () => api.power(props.device.id!, action), 'Готово')
}

async function act(what: string, run: () => Promise<unknown>, done: string) {
  busy.value = what
  try {
    const said = await run()
    emit('say', typeof said === 'string' && said.trim() ? said : done)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = ''
  }
}

function setRole(role: Role) {
  if (role === s.value?.role) return
  act(`role:${role}`, () => api.setRole(props.device.id, role), `${props.device.name}: ${roles.find(([r]) => r === role)?.[1]}`)
}

function setMode(mode: Mode) {
  if (mode === s.value?.mode) return
  act(`mode:${mode}`, () => api.setMode(mode), `Режим: ${modeLabel(mode)}`)
}

function rename() {
  const name = newName.value.trim()
  if (!name || name === props.device.name) return
  act('rename', () => api.rename(props.device.id, name), `${props.device.name} теперь ${name}`).then(
    () => (newName.value = ''),
  )
}

async function forget() {
  if (!props.device.id) return
  try {
    await api.forget(props.device.id)
    emit('say', `${props.device.name} забыто — здесь и на связанных устройствах`)
    emit('gone')
  } catch (e) {
    emit('say', String(e), true)
  }
}
</script>

<template>
  <section class="device" :class="{ wide: tab === 'screen' }">
    <header>
      <div class="row">
        <span class="dot" :class="tone(s, device.online, version)"></span>
        <h1>{{ device.name }}</h1>
        <span v-if="me" class="muted">это устройство</span>
      </div>
      <p class="muted summary">
        <template v-if="s">{{ osLabel(s.os) }} · ferry {{ s.version }}</template>
        <template v-if="!me"> · {{ device.path }}</template>
      </p>
      <p v-if="s && s.version !== version" class="warning">
        ⚠ ferry {{ s.version }}, а здесь {{ version }} — обнови, чтобы всё работало одинаково
      </p>
    </header>

    <div v-if="!me" class="tabs">
      <button v-for="[id, title] in tabs" :key="id" :class="{ active: tab === id }" @click="tab = id">
        {{ title }}
      </button>
    </div>

    <Screen v-if="!me && tab === 'screen'" :device="device" @say="(t, e) => emit('say', t, e)" />
    <Jobs v-else-if="!me && tab === 'jobs'" :device="device" @say="(t, e) => emit('say', t, e)" />
    <template v-else>
    <div class="card">
      <h3>Роль</h3>
      <p class="muted small">
        Узел принимает команды: на нём собирают и тестируют. Пульт управляет другими. Роль можно менять
        с любого связанного устройства.
      </p>
      <div class="segmented">
        <button
          v-for="[role, title] in roles"
          :key="role"
          :class="{ on: s?.role === role }"
          :disabled="!reachable || !!busy"
          :aria-busy="busy === `role:${role}`"
          @click="setRole(role)"
        >
          {{ title }}
        </button>
      </div>
    </div>

    <div class="card">
      <h3>Режим</h3>
      <template v-if="me">
        <p class="muted small">
          Режим выбирает человек за устройством — здесь или иконкой ferry в трее. Издалека его не поменять.
        </p>
        <div class="segmented">
          <button
            v-for="[mode, title] in modes"
            :key="mode"
            :class="{ on: s?.mode === mode, [mode]: true }"
            :disabled="!!busy"
            :aria-busy="busy === `mode:${mode}`"
            @click="setMode(mode)"
          >
            {{ title }}
          </button>
        </div>
      </template>
      <template v-else>
        <p>{{ s ? modeLabel(s.mode) : '—' }}</p>
        <p class="muted small">Режим выбирает человек за этим устройством — иконкой ferry в трее.</p>
      </template>
    </div>

    <div class="card">
      <h3>Имя</h3>
      <div class="row">
        <input v-model="newName" class="grow" :placeholder="device.name" :disabled="!reachable" @keydown.enter="rename" />
        <button
          :disabled="!reachable || !!busy || !newName.trim()"
          :aria-busy="busy === 'rename'"
          @click="rename"
        >
          Переименовать
        </button>
      </div>
    </div>

    <div v-if="me" class="card">
      <h3>Журнал</h3>
      <p class="muted small">
        Что связанные устройства делали здесь. Пока управляют — ввод, снимки, окна — иконка в трее белая; забрать
        рабочий стол — «Остановить управление» в её меню.
      </p>
      <ul v-if="journal.length" class="journal">
        <li v-for="(a, i) in journal" :key="i">
          <span class="muted">{{ clock(a.at) }}</span>
          <strong>{{ a.who }}</strong>
          <span :class="{ desk: a.desk }">{{ a.what }}</span>
        </li>
      </ul>
      <p v-else class="muted small">Пока ничего.</p>
    </div>

    <p v-if="!me && !device.online" class="card muted">
      Не на связи: выключено, спит или без сети. ferry подключится сам, как только устройство появится —
      в этой сети или через интернет.
    </p>
    <p v-else-if="!me && s?.mode === 'paused'" class="card muted">
      Человек за устройством отключил его от других — пока режим не вернут, команды не пройдут.
    </p>

    <div v-if="!me && !blocked(device)" class="card">
      <div class="row head">
        <h3>Свойства</h3>
        <button :disabled="detailLoading" :aria-busy="detailLoading" @click="loadDetail">Обновить</button>
      </div>
      <table v-if="detail">
        <tr v-for="[label, value] in detail.properties" :key="label">
          <th>{{ label }}</th>
          <td>{{ value }}</td>
        </tr>
      </table>
      <!-- Столько же строк, сколько отдаёт устройство: таблица встаёт на место заглушки. -->
      <table v-else>
        <tr v-for="i in 12" :key="i">
          <th><span class="skeleton short"></span></th>
          <td><span class="skeleton"></span></td>
        </tr>
      </table>
      <ul v-if="detail?.warnings.length" class="warnings">
        <li v-for="w in detail.warnings" :key="w" class="warning">⚠ {{ w }}</li>
      </ul>
    </div>

    <div v-if="!me && s?.os === 'windows' && !blocked(device)" class="card">
      <h3>Питание</h3>
      <div class="row">
        <button
          :disabled="!!busy"
          :aria-busy="busy === 'power:reboot'"
          @click="power('reboot')"
        >
          Перезагрузить
        </button>
        <button
          :disabled="!!busy"
          :aria-busy="busy === 'power:sleep'"
          @click="power('sleep')"
        >
          Усыпить
        </button>
        <button
          class="danger"
          :disabled="!!busy"
          :aria-busy="busy === 'power:shutdown'"
          @click="power('shutdown')"
        >
          Выключить
        </button>
        <button :disabled="!!busy" :aria-busy="busy === 'power:cancel'" @click="power('cancel')">Отменить</button>
      </div>
      <p class="muted small">
        Перезагрузка и выключение — через 10 с, «Отменить» успеет их снять. Уснувшее устройство по сети будится только
        по кабелю.
      </p>
    </div>

    <div v-if="!me" class="row">
      <button class="danger" @click="forget">Забыть устройство</button>
    </div>
    </template>
  </section>
</template>

<style scoped>
/* Во всю высоту main: «Экран» тянет снимок под окно. */
.device {
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 760px;
}

.device.wide {
  max-width: none;
}

.journal {
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 260px;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  font-size: 13px;
  list-style: none;
}

.journal li {
  display: flex;
  gap: 8px;
  min-width: 0;
}

.journal li span:last-child {
  overflow-wrap: anywhere;
}

.journal .desk {
  color: var(--coral);
}

.tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--line);
}

.tabs button {
  border: none;
  border-bottom: 2px solid transparent;
  border-radius: 0;
  background: none;
  padding: 8px 14px;
  color: var(--muted);
}

.tabs button.active {
  color: var(--ink);
  border-bottom-color: var(--coral);
}

.head {
  justify-content: space-between;
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
  margin: 0;
  padding: 0;
  list-style: none;
}

.skeleton.short {
  width: 6em;
}

header h1 {
  font-size: 22px;
}

.summary {
  margin: 4px 0 0 18px;
  min-height: 1lh;
}

header .warning {
  margin: 6px 0 0 18px;
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

.grow {
  flex: 1;
}

.segmented {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.segmented button.on {
  background: var(--coral);
  border-color: var(--coral);
  color: #fff;
}

.segmented button.on.no-desktop {
  background: var(--violet);
  border-color: var(--violet);
}

.segmented button.on.paused {
  background: var(--gray);
  border-color: var(--gray);
}
</style>
