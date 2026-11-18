<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { api, type AdminWindow, type AdminWindowsState } from '../api'

type Action = Parameters<typeof api.adminWindows>[0]
const state = ref<AdminWindowsState | null>(null)
const candidates = ref<AdminWindow[] | null>(null)
const selected = ref<number | null>(null)
const busy = ref<Action | null>(null)
const error = ref<{ action: Action; message: string } | null>(null)

function windowName(window: AdminWindow) {
  return [window.title || 'Окно без заголовка', window.process, `PID ${window.pid}`, `@${window.id}`].join(' · ')
}

function clearChoice() {
  candidates.value = null
  selected.value = null
}

async function run(action: Action) {
  if (busy.value || (action === 'grant' && (selected.value === null || !state.value?.owner))) return
  busy.value = action
  try {
    const previous = state.value?.owner
    const result = await api.adminWindows(action, action === 'grant' ? selected.value! : undefined)
    state.value = result
    if (action === 'choose') {
      candidates.value = result.candidates ?? []
      selected.value = null
    } else if (action !== 'show' || previous?.device !== result.owner?.device || previous?.session !== result.owner?.session) {
      clearChoice()
    }
    if (action !== 'show' || error.value?.action === 'show') error.value = null
  } catch (e) {
    error.value = { action, message: String(e) }
    if (action !== 'revoke') clearChoice()
  } finally {
    busy.value = null
  }
}

let timer = 0
onMounted(() => {
  void run('show')
  timer = window.setInterval(() => void run('show'), 4000)
})
onUnmounted(() => window.clearInterval(timer))
</script>

<template>
  <div class="card admin-windows">
    <h3>Окно администратора</h3>
    <p class="muted small">
      Подтверди запрос UAC лично за этим компьютером и открой нужное окно. Разрешение действует для одного окна
      до окончания текущей брони, максимум 15 минут.
    </p>
    <p v-if="state?.owner">Бронь у <strong>{{ state.owner.device }}</strong> · сессия <code>{{ state.owner.session }}</code></p>
    <p v-else-if="state" class="muted">Сначала пульт или агент должен занять рабочий стол.</p>
    <p v-if="state?.window" role="status">Разрешено: {{ windowName(state.window) }}</p>

    <div class="row wrap">
      <button v-if="state?.window" class="danger" :disabled="!!busy" @click="run('revoke')">Отозвать доступ</button>
      <button v-else :disabled="!!busy || !state?.owner" @click="run('choose')">Выбрать окно администратора</button>
    </div>

    <template v-if="state?.owner && !state.window && candidates">
      <div v-if="candidates.length" class="admin-choice">
        <label>
          Окно
          <select v-model="selected" :disabled="!!busy">
            <option :value="null" disabled>Выбери окно</option>
            <option v-for="window in candidates" :key="window.id" :value="window.id">{{ windowName(window) }}</option>
          </select>
        </label>
        <button :disabled="!!busy || selected === null" @click="run('grant')">Разрешить управление окном администратора</button>
      </div>
      <p v-else class="muted">Подходящих окон администратора пока нет.</p>
    </template>
    <p v-if="error" class="warning" role="alert">{{ error.message }}</p>
  </div>
</template>

<style scoped>
.admin-windows { overflow-wrap: anywhere; }
.admin-choice { display: grid; justify-items: start; gap: 8px; margin-top: 12px; }
.admin-choice label { display: grid; gap: 4px; width: 100%; }
.admin-choice select { width: 100%; min-width: 0; }
</style>
