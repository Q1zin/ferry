<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type NodeRow } from '../api'

const props = defineProps<{ node: NodeRow }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const image = ref('')
const loading = ref(false)
const live = ref(false)
const text = ref('')
const combo = ref('')
const pointer = ref<{ x: number; y: number } | null>(null)
const img = ref<HTMLImageElement | null>(null)

// Рабочий стол трогать можно, только если узел на связи и в режиме «работает».
const blocked = computed(() => {
  if (props.node.paused) return 'Узел на паузе на хабе.'
  if (!props.node.online) return 'Узел не на связи.'
  if (props.node.mode && props.node.mode !== 'работает') {
    return `На узле режим «${props.node.mode}» — рабочий стол трогать нельзя.`
  }
  return ''
})

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
    image.value = await api.screenshot(props.node.name)
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
    await api.input(props.node.name, args)
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

onMounted(shoot)
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
        <button :disabled="loading" @click="shoot">{{ loading ? 'Снимаю…' : 'Обновить' }}</button>
        <button :class="{ primary: live }" @click="toggleLive">
          {{ live ? 'Живой экран: вкл' : 'Живой экран' }}
        </button>
        <span class="muted">
          Клик — клик на узле, правый — правый, двойной — двойной.
          <template v-if="pointer"> · {{ pointer.x }}, {{ pointer.y }}</template>
        </span>
      </div>

      <div class="frame">
        <img
          v-if="image"
          ref="img"
          :src="image"
          alt="экран узла"
          @click="onClick"
          @contextmenu.prevent="onRightClick"
          @mousemove="(e) => (pointer = where(e))"
          @mouseleave="pointer = null"
        />
        <p v-else class="muted placeholder">{{ loading ? 'Снимаю экран узла…' : 'Снимка ещё нет' }}</p>
      </div>

      <div class="row">
        <input v-model="text" class="grow" placeholder="Ввести текст на узле" @keydown.enter="typeText" />
        <button :disabled="!text" @click="typeText">Ввести</button>
      </div>
      <div class="row">
        <button v-for="[title, k] in keys" :key="k" @click="send(['key', k])">{{ title }}</button>
        <input v-model="combo" placeholder="своя: ctrl+shift+s" @keydown.enter="pressCombo" />
        <button :disabled="!combo.trim()" @click="pressCombo">Нажать</button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.screen {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.toolbar span {
  margin-left: 4px;
}

.frame {
  border: 1px solid var(--line);
  border-radius: 10px;
  overflow: hidden;
  background: #000;
  min-height: 200px;
  display: flex;
  align-items: center;
  justify-content: center;
}

/* Снимок — целиком в окне, поле ввода и клавиши под ним видны. Пропорции сохраняются без
   полей по краям: пересчёт клика опирается на рамку картинки. */
.frame img {
  display: block;
  width: auto;
  height: auto;
  max-width: 100%;
  max-height: calc(100vh - 290px);
  cursor: crosshair;
}

.placeholder {
  color: #999;
}

.grow {
  flex: 1;
}
</style>
