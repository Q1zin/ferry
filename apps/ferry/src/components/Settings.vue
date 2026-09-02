<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, type Snapshot } from '../api'

defineProps<{ snap: Snapshot }>()
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

// null — ещё не прочитано: не показывать неверное состояние переключателя.
const autostart = ref<boolean | null>(null)

onMounted(async () => {
  try {
    autostart.value = await api.autostart()
  } catch (e) {
    emit('say', String(e), true)
  }
})

async function toggle() {
  const on = !autostart.value
  try {
    await api.setAutostart(on)
    autostart.value = on
  } catch (e) {
    emit('say', String(e), true)
  }
}
</script>

<template>
  <section class="settings">
    <h1>Настройки</h1>

    <div class="card">
      <label class="row">
        <input type="checkbox" :checked="!!autostart" :disabled="autostart === null" @change="toggle" />
        Запускать ferry при входе в систему
      </label>
      <p class="muted small">
        Без этого устройство не на связи, пока ferry не откроют вручную. Окно при этом не появляется — только
        иконка в трее.
      </p>
    </div>

    <div class="card">
      <h3>О ferry</h3>
      <p>Версия {{ snap.me.version }}. На связанных устройствах она должна совпадать.</p>
      <p class="muted small">
        Ключ этого устройства: <code>{{ snap.id }}</code>
      </p>
      <p class="muted small">Всё, что делает приложение, умеет и CLI: <code>ferry</code> без аргументов — список команд.</p>
    </div>
  </section>
</template>

<style scoped>
.settings {
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

.small {
  font-size: 12px;
}

code {
  overflow-wrap: anywhere;
}
</style>
