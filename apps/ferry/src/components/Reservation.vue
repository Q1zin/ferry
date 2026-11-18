<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, type DeskReservation } from '../api'

const props = defineProps<{ id: string | null; canReserve: boolean }>()
const reservation = ref<DeskReservation | null>(null)
const minutes = ref(30)
const busy = ref(false)
const error = ref('')
const valid = computed(() => Number.isInteger(minutes.value) && minutes.value >= 1 && minutes.value <= 720)

async function update(action: 'show' | 'take' | 'free') {
  if (busy.value) return
  busy.value = true
  try {
    reservation.value = await api.deskReservation(props.id, action, action === 'take' ? minutes.value : undefined)
    if (action !== 'show') error.value = ''
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = false
  }
}

let timer: ReturnType<typeof setInterval>
onMounted(() => {
  update('show')
  timer = setInterval(() => update('show'), 10_000)
})
onUnmounted(() => clearInterval(timer))
</script>

<template>
  <section class="card">
    <h3>Бронь без сна</h3>
    <p v-if="reservation?.state === 'busy'" class="muted small" aria-live="polite">
      {{ reservation.owned ? 'Бронь у этого пульта' : `Бронь у ${reservation.owner?.device}` }}<template v-if="reservation.keep_awake"> · удержание питания ещё {{ Math.ceil((reservation.awake_remaining_seconds ?? 0) / 60) }} мин</template>
    </p>
    <p v-else class="muted small">Система и экран остаются включёнными во время брони.</p>
    <div class="row">
      <label>Минуты <input v-model.number="minutes" type="number" min="1" max="720" step="1" /></label>
      <button
        :disabled="busy || !reservation || !canReserve || !valid || (reservation.state === 'busy' && !reservation.owned)"
        @click="update('take')"
      >{{ reservation?.owned && reservation.keep_awake ? 'Продлить бронь' : 'Зарезервировать без сна' }}</button>
      <button v-if="reservation?.owned" :disabled="busy" @click="update('free')">Снять бронь</button>
    </div>
    <p v-if="!canReserve" class="muted small">Для брони включи управление рабочим столом на устройстве.</p>
    <p v-if="error" class="warning" role="alert">{{ error }}</p>
  </section>
</template>
