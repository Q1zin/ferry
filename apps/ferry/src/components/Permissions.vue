<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { api, type Permissions } from '../api'

const emit = defineEmits<{ say: [text: string, error?: boolean]; changed: [p: Permissions] }>()

const state = ref<Permissions | null>(null)

const items = [
  {
    kind: 'screen',
    title: 'Запись экрана',
    what: 'Снимки и запись экрана, заголовки окон — то, чем на этот мак смотрят с других устройств.',
  },
  {
    kind: 'input',
    title: 'Универсальный доступ',
    what: 'Клики, ввод текста и клавиш с других устройств — настоящий ввод, как человек за клавиатурой.',
  },
] as const

async function refresh() {
  try {
    state.value = await api.permissions()
    if (state.value) emit('changed', state.value)
  } catch (e) {
    emit('say', String(e), true)
  }
}

async function ask(kind: 'screen' | 'input') {
  try {
    await api.askPermission(kind)
  } catch (e) {
    emit('say', String(e), true)
  }
}

// Вернулся из «Системных настроек» — окно снова в фокусе: перечитать.
onMounted(() => {
  refresh()
  window.addEventListener('focus', refresh)
})
onUnmounted(() => window.removeEventListener('focus', refresh))
</script>

<template>
  <section class="permissions">
    <h1>Разрешения</h1>
    <p class="muted">
      macOS пускает к экрану и вводу только приложения, которым это разрешили. Выдай оба доступа один раз — обновления
      ferry их сохраняют.
    </p>

    <div v-for="item in items" :key="item.kind" class="card">
      <div class="row head">
        <span class="dot" :class="state ? (state[item.kind] ? 'ok' : 'warn') : 'probing'"></span>
        <h3>{{ item.title }}</h3>
        <span class="muted small">{{ state ? (state[item.kind] ? 'выдано' : 'не выдано') : '' }}</span>
      </div>
      <p class="muted small">{{ item.what }}</p>
      <div v-if="state && !state[item.kind]" class="row">
        <button @click="ask(item.kind)">Выдать</button>
        <span class="muted small">Откроются «Системные настройки» — включи там ferry и вернись сюда.</span>
      </div>
    </div>

    <div v-if="state && !state.screen" class="card">
      <p class="small">
        Включил «Запись экрана», а здесь всё ещё «не выдано»? macOS применит её после перезапуска ferry.
      </p>
      <div class="row">
        <button @click="api.restartApp()">Перезапустить ferry</button>
      </div>
    </div>
  </section>
</template>

<style scoped>
.permissions {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 680px;
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

.head {
  align-items: center;
  gap: 10px;
}

.small {
  font-size: 12px;
}
</style>
