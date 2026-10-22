<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { Channel, invoke } from '@tauri-apps/api/core'
import { api, blocked as why, type DeviceRef } from '../api'

const props = defineProps<{ device: DeviceRef }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const image = ref('')
const loading = ref(false)
const live = ref(false)
const controlling = ref(false)
const following = ref(false)
const fps = ref(0)
const frame = ref<{source_width:number;source_height:number;origin_x:number;origin_y:number}|null>(null)
let streamToken:number|null=null
let generation=0
let lastFrame=0
let heartbeat=0
let pendingMove:{x:number;y:number}|null=null
let moving=false
let controlTail:Promise<unknown>=Promise.resolve()
let controlGeneration=0
function control(args:string[]|null, epoch=controlGeneration) {
  const id=props.device.id
  const operation=controlTail.catch(()=>{}).then(()=>{
    if(args!==null && epoch!==controlGeneration) return
    return invoke('control_screen',{id,args})
  })
  controlTail=operation
  return operation
}
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
    frame.value=null
  } catch (e) {
    emit('say', String(e), true)
    live.value = false
  } finally {
    loading.value = false
  }
}

async function stopLive() {
  generation++
  live.value=false
  const token=streamToken
  streamToken=null
  if (token!==null) await invoke('stop_stream',{token}).catch(()=>{})
}
async function toggleLive() {
  if (live.value) { await stopLive(); return }
  const current=++generation
  live.value=true
  const channel=new Channel<{frame?:{jpeg:string;source_width:number;source_height:number;origin_x:number;origin_y:number};error?:string}>()
  channel.onmessage=(event)=>{
    if (current!==generation) return
    if (event.error) { emit('say',event.error,true); stopLive(); return }
    if (!event.frame) return
    frame.value=event.frame
    const next='data:image/jpeg;base64,'+event.frame.jpeg
    if(image.value===next) acknowledge()
    else image.value=next
    const now=performance.now()
    if(lastFrame) fps.value=Math.round(10000/(now-lastFrame))/10
    lastFrame=now
  }
  try {
    const token=await invoke<number>('start_stream',{id:props.device.id,channel})
    if(current!==generation) { await invoke('stop_stream',{token}); return }
    streamToken=token
    // The first image may finish decoding before start_stream returns.
    if(img.value?.complete) await acknowledge()
  } catch(e) { if(current===generation) {emit('say',String(e),true);await stopLive()} }
}
async function acknowledge() { if(streamToken!==null) await invoke('stream_ack',{token:streamToken}).catch(()=>{}) }
async function stopControl() {
  controlGeneration++
  controlling.value=false
  following.value=false
  pendingMove=null
  clearInterval(heartbeat)
  await control(null).catch(()=>{})
}
async function toggleControl() {
  if(controlling.value) return stopControl()
  const epoch=++controlGeneration
  try {
    await control([],epoch)
    if(epoch!==controlGeneration) return
    controlling.value=true
    heartbeat=window.setInterval(()=>{if(controlling.value) control([]).catch(e=>{emit('say',String(e),true);stopControl()})},10000)
  } catch(e) {emit('say',String(e),true)}
}
function pointerMove(event:MouseEvent) {
  pointer.value=where(event)
  if(!controlling.value || !following.value || !pointer.value) return
  pendingMove=pointer.value
  if(!moving) flushMove()
}
async function flushMove() {
  moving=true
  while(pendingMove && controlling.value && following.value) {
    const p=pendingMove;pendingMove=null
    await send(['move',String(p.x),String(p.y)])
  }
  moving=false
}

async function send(args: string[]) {
  if(!controlling.value || blocked.value) return
  try {
    await control(args)
    // Показать результат: снимок чуть погодя, когда узел успеет отрисовать.
    if (!live.value) window.setTimeout(shoot, 600)
  } catch (e) {
    emit('say', String(e), true)
    await stopControl()
  }
}

/// Координаты клика на картинке → пиксели экрана узла.
function where(event: MouseEvent) {
  const el = img.value
  if (!el || !el.naturalWidth) return null
  const box = el.getBoundingClientRect()
  return {
    x: (frame.value?.origin_x ?? 0)+Math.min((frame.value?.source_width ?? el.naturalWidth)-1,Math.max(0,Math.floor(((event.clientX-box.left)*(frame.value?.source_width ?? el.naturalWidth))/box.width))),
    y: (frame.value?.origin_y ?? 0)+Math.min((frame.value?.source_height ?? el.naturalHeight)-1,Math.max(0,Math.floor(((event.clientY-box.top)*(frame.value?.source_height ?? el.naturalHeight))/box.height))),
  }
}

// Одиночный клик ждёт, не будет ли второго: иначе двойной ушёл бы как два одиночных и двойной.
let clickTimer = 0
function onClick(event: MouseEvent) {
  const p = where(event)
  if (!p || !controlling.value) return
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
    if (b) { stopLive(); stopControl() }
    else if (!image.value) shoot()
  },
  { immediate: true },
)
onUnmounted(() => {
  stopLive()
  stopControl()
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
        <button :class="{primary:controlling}" :aria-pressed="controlling" @click="toggleControl">{{controlling ? 'Отключить управление' : 'Управлять'}}</button>
        <label v-if="controlling"><input v-model="following" type="checkbox" /> Курсор за мышью</label>
        <span class="muted">{{live ? `${fps} кадров/с · основной экран` : 'Просмотр'}} · {{controlling ? 'Управление включено' : 'Ввод выключен'}}</span>
      </div>

      <div class="frame">
        <img
          v-if="image"
          ref="img"
          :src="image"
          alt="экран устройства"
          @click="onClick"
          @contextmenu.prevent="onRightClick"
          @load="acknowledge"
          @mousemove="pointerMove"
          @mouseleave="pointer = null; pendingMove = null"
        />
        <p v-else class="muted placeholder">{{ loading ? 'Снимаю экран устройства…' : 'Снимка ещё нет' }}</p>
        <span v-if="pointer" class="pointer">{{ pointer.x }}, {{ pointer.y }}</span>
      </div>

      <div v-if="controlling" class="row">
        <input v-model="text" class="grow" placeholder="Ввести текст на устройстве" @keydown.enter="typeText" />
        <button :disabled="!text" @click="typeText">Ввести</button>
        <input v-model="combo" class="combo" placeholder="клавиши: ctrl+shift+s" @keydown.enter="pressCombo" />
        <button :disabled="!combo.trim()" @click="pressCombo">Нажать</button>
      </div>
      <div v-if="controlling" class="row keys">
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
