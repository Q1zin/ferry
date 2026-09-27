<script setup lang="ts">
import { ref } from 'vue'
import { api } from '../api'

const emit = defineEmits<{ say: [text: string, error?: boolean]; added: [name: string] }>()

const name = ref('')
const target = ref('')
const busy = ref(false)

async function add() {
  const n = name.value.trim()
  const t = target.value.trim()
  if (!n || !t.includes('@')) return
  busy.value = true
  try {
    await api.addNode(n, t)
    emit('say', `${n} записан — проверяю связь`)
    emit('added', n)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section class="add">
    <h1>Добавить узел</h1>

    <div class="card">
      <h3>1. Поставь ferry на узел</h3>
      <p class="muted">
        На Windows-машине, в PowerShell от администратора, под тем пользователем, на чьём рабочем
        столе будут идти команды, — установщик <code>node/install.ps1</code> из репозитория ferry
        (как раздать его с хаба — в README). В конце он напечатает пользователя и адреса узла.
      </p>
    </div>

    <div class="card">
      <h3>2. Запиши узел здесь</h3>
      <div class="row">
        <input v-model="name" placeholder="имя: win-11" />
        <input v-model="target" class="grow" placeholder="пользователь@адрес" @keydown.enter="add" />
        <button class="primary" :disabled="busy || !name.trim() || !target.includes('@')" @click="add">
          Добавить
        </button>
      </div>
      <p class="muted small">
        Ключ хаба — <code>~/.ssh/ferry</code>. Если узел потом сменит адрес, ferry найдёт его по ключу
        хоста сам.
      </p>
    </div>
  </section>
</template>

<style scoped>
.add {
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

.grow {
  flex: 1;
}

.small {
  font-size: 12px;
}
</style>
