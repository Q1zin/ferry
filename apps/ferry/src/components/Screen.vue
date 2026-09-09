<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { api, blocked as why, type DeviceRef } from '../api'

const props = defineProps<{ device: DeviceRef }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const image = ref('')
const loading = ref(false)
const live = ref(false)
const text = ref('')
const combo = ref('')
const pointer = ref<{ x: number; y: number } | null>(null)
const img = ref<HTMLImageElement | null>(null)

// Рабочий стол трогать можно, только если устройство на связи и в режиме «работает».
const blocked = computed(() => why(props.device, true))

const keys = [
  ['Enter', 'enter'],
  ['Esc', 'esc'],
  ['Tab', 'tab'],
  ['⌫', 'backspace'],
  ['Ctrl+A', 'ctrl+a'],
  ['Ctrl+C', 'ctrl+c'],
  ['Ctrl+V', 'ctrl+v'],
  ['Alt+Tab', 'alt+tab'],
  ['Win+D', 'win+d'],
] as const

async function shoot() {
  if (loading.value || blocked.value) return
  loading.value = true
  try {
    image.value = await api.screenshot(props.device.id!)
  } catch (e) {
    emit('say', String(e), true)
    live.value = false
  } finally {
    loading.value = false
  }
}

// Живой экран: следующий снимок — после того, как пришёл предыдущий, запросы не копятся.
let liveTimer = 0
async function tick() {
  if (!live.value) return
  await shoot()
  if (live.value) liveTimer = window.setTimeout(tick, 1500)
}
function toggleLive() {
  live.value = !live.value
  clearTimeout(liveTimer)
  if (live.value) tick()
}

async function send(args: string[]) {
  try {
    await api.input(props.device.id!, args)
    // Показать результат: снимок чуть погодя, когда узел успеет отрисовать.
    if (!live.value) window.setTimeout(shoot, 600)
  } catch (e) {
    emit('say', String(e), true)
  }
}

/// Координаты клика на картинке → пиксели экрана узла.
function where(event: MouseEvent) {
  const el = img.value
  if (!el || !el.naturalWidth) return null
  const box = el.getBoundingClientRect()
  return {
    x: Math.round(((event.clientX - box.left) * el.naturalWidth) / box.width),
    y: Math.round(((event.clientY - box.top) * el.naturalHeight) / box.height),
  }
}

// Одиночный клик ждёт, не будет ли второго: иначе двойной ушёл бы как два одиночных и двойной.
let clickTimer = 0
function onClick(event: MouseEvent) {
  const p = where(event)
  if (!p) return
  if (clickTimer) {
    clearTimeout(clickTimer)
    clickTimer = 0
    send(['click', String(p.x), String(p.y), 'double'])
    return
  }
  clickTimer = window.setTimeout(() => {
    clickTimer = 0
    send(['click', String(p.x), String(p.y)])
  }, 250)
}

function onRightClick(event: MouseEvent) {
  const p = where(event)
  if (p) send(['click', String(p.x), String(p.y), 'right'])
}

function typeText() {
  if (!text.value) return
  send(['type', text.value])
  text.value = ''
}

function pressCombo() {
  const c = combo.value.trim()
  if (!c) return
  send(['key', ...c.split(/\s+/)])
  combo.value = ''
}

// Первый снимок — как только узел ответил на опрос и разрешает рабочий стол.
watch(
  blocked,
  (b) => {
    if (!b && !image.value) shoot()
  },
  { immediate: true },
)
onUnmounted(() => {
  live.value = false
  clearTimeout(liveTimer)
  clearTimeout(clickTimer)
})
</script>

<template>
  <div class="screen">
    <p v-if="blocked" class="card muted">{{ blocked }}</p>
    <template v-else>
      <div class="row toolbar">
        <button :disabled="loading || live" :aria-busy="loading && !live" @click="shoot">Обновить</button>
        <button :class="{ primary: live }" :aria-pressed="live" @click="toggleLive">Живой экран</button>
        <span class="muted">Клики по снимку уходят на устройство.</span>
      </div>

      <div class="frame">
        <img
          v-if="image"
          ref="img"
          :src="image"
          alt="экран устройства"
          @click="onClick"
          @contextmenu.prevent="onRightClick"
          @mousemove="(e) => (pointer = where(e))"
          @mouseleave="pointer = null"
        />
        <p v-else class="muted placeholder">{{ loading ? 'Снимаю экран устройства…' : 'Снимка ещё нет' }}</p>
        <span v-if="pointer" class="pointer">{{ pointer.x }}, {{ pointer.y }}</span>
      </div>

      <div class="row">
        <input v-model="text" class="grow" placeholder="Ввести текст на устройстве" @keydown.enter="typeText" />
        <button :disabled="!text" @click="typeText">Ввести</button>
        <input v-model="combo" class="combo" placeholder="клавиши: ctrl+shift+s" @keydown.enter="pressCombo" />
        <button :disabled="!combo.trim()" @click="pressCombo">Нажать</button>
      </div>
      <div class="row keys">
        <button v-for="[title, k] in keys" :key="k" @click="send(['key', k])">{{ title }}</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* Снимок занимает остаток высоты окна, но не меньше 220px. Рамка не зависит от снимка:
   пришёл он или нет, поле ввода и клавиши под ней не двигаются. */
.screen {
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.toolbar span {
  margin-left: 4px;
}

.frame {
  position: relative;
  flex: 1 1 220px;
  min-height: 220px;
  border: 1px solid var(--line);
  border-radius: 10px;
  overflow: hidden;
  background: #000;
}

/* Вписан в рамку по центру с сохранением пропорций и без полей внутри картинки:
   пересчёт клика опирается на её прямоугольник. */
.frame img {
  position: absolute;
  inset: 0;
  margin: auto;
  max-width: 100%;
  max-height: 100%;
  cursor: crosshair;
}

.placeholder {
  position: absolute;
  inset: 0;
  margin: 0;
  display: grid;
  place-items: center;
  color: #999;
}

.pointer {
  position: absolute;
  left: 8px;
  bottom: 8px;
  padding: 2px 8px;
  border-radius: 6px;
  background: rgb(0 0 0 / 0.6);
  color: #fff;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  pointer-events: none;
}

.grow {
  flex: 1;
}

.combo {
  width: 170px;
}

/* Компактные: в минимальном окне все клавиши — одной строкой. */
.keys {
  gap: 6px;
}

.keys button {
  padding: 4px 9px;
  font-size: 13px;
}
</style>
