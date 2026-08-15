<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api } from '../api'

const emit = defineEmits<{ say: [text: string, error?: boolean] }>()

const hosts = ref<string[]>([])
const host = ref('')
const version = ref('')

async function load() {
  try {
    hosts.value = await api.trusted()
    version.value = await api.version()
  } catch (e) {
    emit('say', String(e), true)
  }
}

async function add() {
  const h = host.value.trim()
  if (!h) return
  try {
    await api.trust(h)
    emit('say', `git с ${h} на узлах пойдёт через ключи этого хаба (только чтение)`)
    host.value = ''
    await load()
  } catch (e) {
    emit('say', String(e), true)
  }
}

onMounted(load)
</script>

<template>
  <section class="settings">
    <h1>Настройки</h1>

    <div class="card">
      <h3>Приватные git-зависимости</h3>
      <p class="muted">
        Хосты, к которым git на узлах ходит через ключи этого хаба: ключ не покидает хаб, релей
        пропускает только чтение.
      </p>
      <ul v-if="hosts.length">
        <li v-for="h in hosts" :key="h"><code>{{ h }}</code></li>
      </ul>
      <p v-else class="muted">Доверенных хостов нет.</p>
      <div class="row">
        <input v-model="host" placeholder="git.example.com" @keydown.enter="add" />
        <button :disabled="!host.trim()" @click="add">Доверить</button>
      </div>
    </div>

    <div class="card">
      <h3>О ferry</h3>
      <p>Версия {{ version || '…' }}. На узлах она должна совпадать — иначе узел предупредит в «Обзоре».</p>
      <p class="muted">
        Всё, что делает пульт, умеет и CLI: <code>ferry</code> без аргументов — список команд.
      </p>
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

ul {
  margin: 0;
  padding-left: 18px;
}
</style>
