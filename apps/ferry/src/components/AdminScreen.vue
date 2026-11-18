<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type AdminFrame, type AdminWindow } from '../api'

const props = defineProps<{ id: string }>()
const emit = defineEmits<{ close: [] }>()

const windows = ref<AdminWindow[] | null>(null)
const selection = ref('')
const frame = ref<AdminFrame | null>(null)
const picture = ref<HTMLImageElement | null>(null)
const revision = ref(0)
const ready = ref(false)
const busy = ref(false)
const leaving = ref(false)
const error = ref('')
const text = ref('')
const combo = ref('')
let releasing: Promise<null> | undefined
let disposed = false

const windowKey = ({ id, pid }: AdminWindow) => pid + ':' + id
const selected = computed(() => windows.value?.find(window => windowKey(window) === selection.value))

function windowName(window: AdminWindow) {
  return [window.title || 'Окно без заголовка', window.process, 'PID ' + window.pid, '@' + window.id].join(' · ')
}

function clearFrame() {
  ready.value = false
  frame.value = null
}

function run(action: () => Promise<void>) {
  if (busy.value || leaving.value || disposed) return
  busy.value = true
  error.value = ''
  void action().catch(e => {
    if (disposed) return
    clearFrame()
    error.value = String(e)
  }).finally(() => {
    busy.value = false
  })
}

async function capture(window: AdminWindow) {
  clearFrame()
  const result = await api.adminScreen(props.id, 'shot', { id: window.id, pid: window.pid })
  if (disposed || leaving.value) return
  if (windowKey(result.window) !== windowKey(window)) throw new Error('Окно изменилось. Обнови список и выбери его снова.')
  const rect = result.rect
  if (rect.length !== 4 || !rect.every(Number.isSafeInteger) || rect[2] <= 0 || rect[3] <= 0) {
    throw new Error('Устройство вернуло неверную геометрию окна.')
  }
  frame.value = result
  revision.value++
}

function refreshWindows() {
  run(async () => {
    clearFrame()
    windows.value = null
    const available = await api.adminScreen(props.id, 'list')
    if (disposed || leaving.value) return
    windows.value = available
    if (selected.value) await capture(selected.value)
    else selection.value = ''
  })
}

function refreshFrame() {
  const window = selected.value
  if (window) run(() => capture(window))
  else clearFrame()
}

function imageLoaded(event: Event) {
  const image = picture.value
  if (!image || !frame.value || event.currentTarget !== image) return
  const [, , width, height] = frame.value.rect
  if (image.naturalWidth !== width || image.naturalHeight !== height) {
    clearFrame()
    error.value = 'Размер снимка не совпал с геометрией окна.'
    return
  }
  ready.value = true
}

function imageFailed(event: Event) {
  if (event.currentTarget !== picture.value) return
  clearFrame()
  error.value = 'Не удалось показать снимок окна. Обнови снимок.'
}

function focus() {
  const window = selected.value
  if (!window) return
  run(async () => {
    await api.adminScreen(props.id, 'focus', { id: window.id, pid: window.pid })
    await captureAfterAction(window)
  })
}

async function captureAfterAction(window: AdminWindow) {
  if (disposed || leaving.value) return
  try {
    await capture(window)
  } catch (e) {
    throw new Error('Действие выполнено, снимок не обновился. ' + String(e))
  }
}

function send(args: string[], after?: () => void) {
  const window = selected.value
  const current = frame.value
  if (!window || !current || !ready.value) return
  run(async () => {
    await api.adminScreen(props.id, 'input', { id: window.id, pid: window.pid }, args, current.rect)
    after?.()
    await captureAfterAction(window)
  })
}

function click(event: MouseEvent, right = false) {
  const image = picture.value
  const current = frame.value
  if (!image || !current || event.currentTarget !== image || (!right && event.detail > 1)) return
  const box = image.getBoundingClientRect()
  if (box.width <= 0 || box.height <= 0) return
  const [x, y, width, height] = current.rect
  const px = x + Math.max(0, Math.min(width - 1, Math.floor((event.clientX - box.left) * width / box.width)))
  const py = y + Math.max(0, Math.min(height - 1, Math.floor((event.clientY - box.top) * height / box.height)))
  send(['click', String(px), String(py), ...(right ? ['right'] : [])])
}

function typeText() {
  if (text.value) send(['type', text.value], () => { text.value = '' })
}

function pressCombo() {
  const keys = combo.value.trim()
  if (keys) send(['key', keys], () => { combo.value = '' })
}

function release() {
  return releasing ??= api.adminScreen(props.id, 'release')
}

async function close() {
  if (leaving.value) return
  leaving.value = true
  try {
    await release()
    if (!disposed) emit('close')
  } catch (e) {
    if (!disposed) {
      error.value = String(e)
      clearFrame()
      releasing = undefined
      leaving.value = false
    }
  }
}

onMounted(refreshWindows)
onUnmounted(() => {
  disposed = true
  void release().catch(() => {})
})
</script>

<template>
  <div class="admin-screen">
    <div class="row">
      <h3>Управление окном администратора</h3>
      <button :disabled="leaving" @click="close">Вернуться к обычному экрану</button>
    </div>
    <p v-if="!windows && error" class="muted">
      Чтобы дать доступ, за этим Windows-компьютером открой Ferry → «Это устройство» → «Окно администратора»
      и выдай разрешение этому пульту. Затем обнови список. Системные запросы Windows подтверждают за компьютером.
    </p>
    <div class="row">
      <button :disabled="busy || leaving" @click="refreshWindows">Обновить список окон</button>
      <label v-if="windows?.length">
        Окно администратора
        <select v-model="selection" :disabled="busy || leaving" @change="refreshFrame">
          <option value="" disabled>Выбери окно</option>
          <option v-for="window in windows" :key="windowKey(window)" :value="windowKey(window)">{{ windowName(window) }}</option>
        </select>
      </label>
      <span v-else-if="windows" class="muted">Доступных окон администратора пока нет.</span>
    </div>
    <div v-if="selected" class="row">
      <button :disabled="busy || leaving" @click="focus">Активировать окно</button>
      <button :disabled="busy || leaving" @click="refreshFrame">Обновить снимок</button>
      <span v-if="busy" class="muted" role="status">Жду ответа устройства…</span>
    </div>
    <p v-if="error" class="warning" role="alert">{{ error }}</p>

    <template v-if="selected">
      <div class="admin-frame">
        <img
          v-if="frame"
          :key="revision"
          ref="picture"
          :src="frame.image"
          :class="{ interactive: ready && !busy && !leaving }"
          alt="Снимок окна администратора"
          draggable="false"
          @load="imageLoaded"
          @error="imageFailed"
          @click="click($event)"
          @contextmenu.prevent="click($event, true)"
        />
        <p v-else class="muted">{{ busy ? 'Получаю снимок окна…' : 'Снимка окна пока нет' }}</p>
      </div>
      <div class="row">
        <input v-model="text" :disabled="!ready || busy || leaving" placeholder="Текст для окна администратора" @keydown.enter="typeText" />
        <button :disabled="!ready || busy || leaving || !text" @click="typeText">Ввести текст</button>
        <input v-model="combo" :disabled="!ready || busy || leaving" placeholder="Клавиши: ctrl+shift+s" @keydown.enter="pressCombo" />
        <button :disabled="!ready || busy || leaving || !combo.trim()" @click="pressCombo">Нажать клавиши</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.admin-screen {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
  overflow-wrap: anywhere;
}

.admin-screen p { margin: 0; }
.admin-screen label { display: grid; min-width: 0; max-width: 100%; gap: 4px; }
.admin-screen select { width: 100%; }

.admin-frame {
  position: relative;
  display: grid;
  flex: 1 1 220px;
  min-height: 220px;
  place-items: center;
  overflow: hidden;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: #000;
}

.admin-frame img {
  position: absolute;
  inset: 0;
  margin: auto;
  max-width: 100%;
  max-height: 100%;
}

.admin-frame .interactive { cursor: crosshair; }
</style>
