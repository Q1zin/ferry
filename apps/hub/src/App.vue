<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, tone, type NodeRow } from './api'
import logo from '../src-tauri/icons/128x128.png'
import AddNode from './components/AddNode.vue'
import NodeView from './components/NodeView.vue'
import Settings from './components/Settings.vue'

type View = { kind: 'home' } | { kind: 'node'; name: string } | { kind: 'settings' } | { kind: 'add' }

const nodes = ref<NodeRow[]>([])
const loading = ref(false)
const loadedOnce = ref(false)
const view = ref<View>({ kind: 'home' })
const notice = ref<{ text: string; error: boolean } | null>(null)

const selected = computed(() => {
  const v = view.value
  return v.kind === 'node' ? nodes.value.find((n) => n.name === v.name) : undefined
})

async function refresh() {
  if (loading.value) return
  loading.value = true
  try {
    nodes.value = await api.nodes()
  } catch (e) {
    say(String(e), true)
  } finally {
    loading.value = false
    loadedOnce.value = true
  }
}

let noticeTimer = 0
function say(text: string, error = false) {
  if (!text.trim()) return
  notice.value = { text: text.trim(), error }
  clearTimeout(noticeTimer)
  noticeTimer = window.setTimeout(() => (notice.value = null), error ? 10000 : 5000)
}

function open(name: string) {
  view.value = { kind: 'node', name }
}

// Статусы — раз в 15 с: каждый опрос стоит ssh к каждому узлу.
let timer = 0
onMounted(() => {
  refresh()
  timer = window.setInterval(refresh, 15000)
})
onUnmounted(() => clearInterval(timer))
</script>

<template>
  <div class="app">
    <aside>
      <button class="brand" @click="view = { kind: 'home' }">
        <img :src="logo" alt="" />
        <span>
          <strong>ferry</strong>
          <span class="muted">пульт узлов</span>
        </span>
      </button>

      <nav>
        <button
          v-for="node in nodes"
          :key="node.name"
          class="node-link"
          :class="{ active: view.kind === 'node' && view.name === node.name }"
          @click="open(node.name)"
        >
          <span class="dot" :class="tone(node)"></span>
          <span class="name">{{ node.name }}</span>
          <span v-if="node.warnings.length" class="warning">⚠ {{ node.warnings.length }}</span>
        </button>
        <p v-if="loadedOnce && !nodes.length" class="muted hint">Узлов пока нет</p>
      </nav>

      <footer>
        <button @click="view = { kind: 'add' }">+ Добавить узел</button>
        <div class="row">
          <button class="grow" @click="view = { kind: 'settings' }">Настройки</button>
          <button :disabled="loading" title="Обновить статусы" @click="refresh">
            {{ loading ? '…' : '↻' }}
          </button>
        </div>
      </footer>
    </aside>

    <main>
      <NodeView
        v-if="view.kind === 'node' && selected"
        :key="selected.name"
        :node="selected"
        @say="say"
        @changed="refresh"
        @renamed="(name: string) => { refresh().then(() => open(name)) }"
        @gone="view = { kind: 'home' }; refresh()"
      />
      <Settings v-else-if="view.kind === 'settings'" @say="say" />
      <AddNode
        v-else-if="view.kind === 'add'"
        @say="say"
        @added="(name: string) => { refresh().then(() => open(name)) }"
      />
      <section v-else class="home">
        <h1>Узлы</h1>
        <p v-if="!loadedOnce" class="muted">Опрашиваю узлы…</p>
        <p v-else-if="!nodes.length" class="muted">
          Добавь первый узел: поставь на нём ferry (node/install.ps1) и нажми «Добавить узел».
        </p>
        <div class="cards">
          <button v-for="node in nodes" :key="node.name" class="card tile" @click="open(node.name)">
            <div class="row">
              <span class="dot" :class="tone(node)"></span>
              <strong>{{ node.name }}</strong>
              <span class="muted">{{ node.addr }}</span>
              <span v-if="node.latency_ms != null" class="muted latency">{{ node.latency_ms }} мс</span>
            </div>
            <p class="summary">{{ node.summary }}</p>
            <ul v-if="node.warnings.length" class="warnings">
              <li v-for="w in node.warnings" :key="w" class="warning">⚠ {{ w }}</li>
            </ul>
          </button>
        </div>
      </section>
    </main>

    <div v-if="notice" class="notice" :class="{ error: notice.error }" @click="notice = null">
      {{ notice.text }}
    </div>
  </div>
</template>

<style scoped>
.app {
  display: grid;
  grid-template-columns: 230px 1fr;
  height: 100vh;
}

aside {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 14px 12px;
  border-right: 1px solid var(--line);
  background: color-mix(in srgb, var(--panel) 60%, var(--paper));
}

.brand {
  display: flex;
  gap: 10px;
  align-items: center;
  border: none;
  background: none;
  padding: 2px 4px;
  text-align: left;
}

.brand img {
  width: 36px;
  height: 36px;
}

.brand span {
  display: flex;
  flex-direction: column;
  line-height: 1.2;
}

nav {
  flex: 1;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.node-link {
  display: flex;
  gap: 8px;
  align-items: center;
  border: none;
  background: none;
  text-align: left;
  padding: 8px 10px;
}

.node-link.active,
.node-link:hover {
  background: var(--panel);
}

.node-link .name {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
}

.hint {
  padding: 8px 10px;
}

footer {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.grow {
  flex: 1;
}

main {
  overflow: auto;
  padding: 24px 28px;
}

.home h1 {
  margin-bottom: 16px;
}

.cards {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 12px;
}

.tile {
  text-align: left;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.latency {
  margin-left: auto;
}

.summary {
  margin: 0;
  color: var(--muted);
}

.warnings {
  margin: 0;
  padding: 0;
  list-style: none;
}

.notice {
  position: fixed;
  right: 20px;
  bottom: 20px;
  max-width: 480px;
  padding: 10px 14px;
  border-radius: 10px;
  background: var(--ink);
  color: var(--paper);
  white-space: pre-wrap;
  cursor: pointer;
  box-shadow: 0 6px 24px rgb(0 0 0 / 0.18);
}

.notice.error {
  background: var(--red);
  color: #fff;
}
</style>
