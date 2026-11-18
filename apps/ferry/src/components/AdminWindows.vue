<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type AdminWindow, type AdminWindowsState } from '../api'

type Action = Parameters<typeof api.adminWindows>[0]
const state = ref<AdminWindowsState | null>(null)
const candidates = ref<AdminWindow[] | null>(null)
const selected = ref<number | null>(null)
const busy = ref<Action | null>(null)
const error = ref<{ action: Action; message: string } | null>(null)
const granted = computed(() => state.value?.all_windows === true || !!state.value?.window)
const confirming = computed(() => state.value?.pending || busy.value === 'grant' || busy.value === 'grant_all')
const revoking = computed(() => state.value?.revoking || busy.value === 'revoke')

function windowName(window: AdminWindow) {
  return [window.title || 'Окно без заголовка', window.process, `PID ${window.pid}`, `@${window.id}`].join(' · ')
}

function clearChoice() {
  candidates.value = null
  selected.value = null
}

async function run(action: Action) {
  const choosingAccess = action === 'choose' || action === 'grant' || action === 'grant_all'
  if (busy.value || (choosingAccess && (!state.value?.owner || granted.value || confirming.value || revoking.value))) return
  if (action === 'grant' && selected.value === null) return
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
      Доступ ко всем окнам администратора действует для выбранного пульта до отзыва или выхода из Ferry на этом
      компьютере. Новые сессии агента на том же пульте используют это разрешение. Для управления нужна активная
      бронь этого пульта и разблокированный экран.
    </p>
    <p class="muted small">
      Если нужен доступ только к одному окну, сначала запусти приложение от имени администратора. Разрешение на одно
      окно действует до конца текущей брони, максимум 15 минут. Подтверждай запросы Windows лично за этим компьютером.
    </p>
    <p v-if="state?.owner">Бронь у <strong>{{ state.owner.device }}</strong> · сессия <code>{{ state.owner.session }}</code></p>
    <p v-else-if="state && !granted" class="muted">Сначала пульт или агент должен занять рабочий стол.</p>
    <p v-else-if="state" class="muted">Сейчас рабочий стол свободен.</p>
    <p v-if="state?.all_windows" role="status">
      Разрешено: все окна администратора · пульт <strong>{{ state.approved_controller }}</strong>
    </p>
    <p v-else-if="state?.window" role="status">Разрешено: {{ windowName(state.window) }}</p>
    <p v-if="confirming" role="status">Ожидается подтверждение Windows</p>
    <p v-if="revoking" role="status">Отзыв доступа ещё не завершён</p>

    <div class="row wrap">
      <button v-if="revoking" class="danger" :disabled="!!busy" @click="run('revoke')">Завершить отзыв доступа</button>
      <button v-else-if="granted" class="danger" :disabled="!!busy" @click="run('revoke')">Отозвать доступ</button>
      <template v-else>
        <button class="primary" :disabled="!!busy || !state?.owner || confirming" @click="run('grant_all')">Разрешить для всех окон администратора</button>
        <button :disabled="!!busy || !state?.owner || confirming" @click="run('choose')">Выбрать окно администратора</button>
      </template>
    </div>

    <template v-if="state?.owner && !granted && candidates && !revoking">
      <div v-if="candidates.length" class="admin-choice">
        <label>
          Окно
          <select v-model="selected" :disabled="!!busy || confirming">
            <option :value="null" disabled>Выбери окно</option>
            <option v-for="window in candidates" :key="window.id" :value="window.id">{{ windowName(window) }}</option>
          </select>
        </label>
        <button :disabled="!!busy || confirming || selected === null" @click="run('grant')">Разрешить управление окном администратора</button>
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
