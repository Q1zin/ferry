<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { api, type Snapshot } from '../api'

const props = defineProps<{ snap: Snapshot }>()
const emit = defineEmits<{ say: [text: string, error?: boolean]; paired: [id: string] }>()

const code = ref('')
const pairing = ref(false)
const opening = ref(false)

// Обратный отсчёт до конца окна связывания — считаем сами, агент шлёт события только при изменениях.
const deadline = ref(0)
const now = ref(Date.now())
const timer = window.setInterval(() => (now.value = Date.now()), 1000)
onUnmounted(() => clearInterval(timer))
watch(
  () => props.snap.pairing?.code,
  () => (deadline.value = props.snap.pairing ? Date.now() + props.snap.pairing.seconds_left * 1000 : 0),
  { immediate: true },
)
const left = computed(() => Math.max(0, Math.round((deadline.value - now.value) / 1000)))
const open = computed(() => props.snap.pairing && left.value > 0)
const clock = computed(() => `${Math.floor(left.value / 60)}:${String(left.value % 60).padStart(2, '0')}`)

const waiting = computed(() => props.snap.lan.filter((d) => d.pairing))
const nearby = computed(() => props.snap.lan.filter((d) => !d.pairing))

async function show() {
  opening.value = true
  try {
    await api.openPairing()
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    opening.value = false
  }
}

async function copy() {
  try {
    await navigator.clipboard.writeText(props.snap.pairing!.ticket)
    emit('say', 'Ссылка скопирована — отправь её на другое устройство')
  } catch {
    emit('say', 'Скопировать не вышло — выдели ссылку и скопируй вручную', true)
  }
}

async function pair() {
  if (!code.value.trim()) return
  pairing.value = true
  try {
    const peer = await api.pair(code.value)
    emit('say', `Связано: ${peer.name}`)
    code.value = ''
    emit('paired', peer.id)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    pairing.value = false
  }
}
</script>

<template>
  <section class="pairing">
    <h1>Связать устройство</h1>
    <p class="muted">
      Связанные устройства доверяют друг другу: видят состояние и управляют друг другом. Связь —
      напрямую в одной сети или через интернет, с шифрованием.
    </p>

    <div class="halves">
      <div class="card">
        <h3>Ввести код с другого устройства</h3>
        <p class="muted small">
          На том устройстве: ferry → «Связать устройство» → «Показать код». Из другой сети вставь ссылку
          целиком.
        </p>
        <div class="row">
          <input v-model="code" class="grow" placeholder="1234-5678 или ferry:…" @keydown.enter="pair" />
          <button class="primary" :disabled="pairing || !code.trim()" :aria-busy="pairing" @click="pair">
            Связать
          </button>
        </div>
        <p v-if="waiting.length" class="small">
          Ждут связывания рядом: <strong>{{ waiting.map((d) => d.name).join(', ') }}</strong>
        </p>
        <p v-else-if="nearby.length" class="muted small">
          Рядом есть ferry ({{ nearby.map((d) => d.name).join(', ') }}), но код там не открыт.
        </p>
      </div>

      <div class="card">
        <h3>Добавить это устройство</h3>
        <template v-if="open">
          <p class="muted small">Введи код на другом устройстве ferry. Код работает {{ clock }}.</p>
          <p class="code">{{ snap.pairing!.code }}</p>
          <div class="row ticket">
            <code>{{ snap.pairing!.ticket }}</code>
            <button @click="copy">Скопировать ссылку</button>
          </div>
          <button @click="api.closePairing()">Отменить</button>
        </template>
        <template v-else>
          <p class="muted small">Покажет код на 10 минут: его вводят на устройстве, которое добавляет это.</p>
          <button :disabled="opening" :aria-busy="opening" @click="show">Показать код</button>
        </template>
      </div>
    </div>
  </section>
</template>

<style scoped>
.pairing {
  display: flex;
  flex-direction: column;
  gap: 14px;
  max-width: 900px;
}

.pairing > p {
  margin: 0;
}

.halves {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
  gap: 16px;
  align-items: start;
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

.card > button {
  align-self: flex-start;
}

.small {
  font-size: 12px;
}

.grow {
  flex: 1;
}

.code {
  font: 600 34px/1.2 ui-monospace, SFMono-Regular, Menlo, monospace;
  letter-spacing: 0.06em;
  color: var(--coral);
}

.ticket code {
  flex: 1;
  min-width: 0;
  font: 11px ui-monospace, SFMono-Regular, Menlo, monospace;
  color: var(--muted);
  overflow-wrap: anywhere;
}
</style>
