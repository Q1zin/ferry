<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, tone, type NodeRow } from './api'
import logo from '../src-tauri/icons/128x128.png'
import AddNode from './components/AddNode.vue'
import NodeView from './components/NodeView.vue'
import Settings from './components/Settings.vue'

type View = { kind: 'home' } | { kind: 'node'; name: string } | { kind: 'settings' } | { kind: 'add' }

const nodes = ref<NodeRow[]>([])
const loadedOnce = ref(false)
const probing = ref(new Set<string>())
const view = ref<View>({ kind: 'home' })
const notice = ref<{ text: string; error: boolean } | null>(null)

const selected = computed(() => {
  const v = view.value
  return v.kind === 'node' ? nodes.value.find((n) => n.name === v.name) : undefined
})

// Реестр — мгновенно, без ssh; статус каждого узла приходит сам по себе. Пока узел
// переопрашивается, виден прежний ответ — карточки не мигают.
async function refresh() {
  try {
    const known = new Map(nodes.value.map((n) => [n.name, n]))
    nodes.value = (await api.nodes()).map((n) => {
      const old = n.paused ? undefined : known.get(n.name)
      return { ...n, status: old?.status ?? null, warnings: old?.warnings ?? null }
    })
  } catch (e) {
    say(String(e), true)
  } finally {
    loadedOnce.value = true
  }
  for (const n of nodes.value) if (!n.paused) probe(n.name)
}

// Статус — один ssh, предупреждения — ещё один, следом. Узел, который ещё отвечает на прошлый
// опрос, не дёргаем: пропавший отвечает секунды.
async function probe(name: string) {
  if (probing.value.has(name)) return
  probing.value.add(name)
  try {
    const status = await api.status(name)
    patch(name, { status })
    if (!status.online) return patch(name, { warnings: [] })
    const detail = await api.detail(name).catch(() => null)
    if (detail) patch(name, { warnings: detail.warnings })
  } catch (e) {
    say(String(e), true)
  } finally {
    probing.value.delete(name)
  }
}

// По имени, а не по ссылке: пока шёл ssh, список могли перечитать.
function patch(name: string, fields: Partial<NodeRow>) {
  const row = nodes.value.find((n) => n.name === name)
  if (row) Object.assign(row, fields)
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
          <span v-if="node.warnings?.length" class="warning">⚠ {{ node.warnings.length }}</span>
        </button>
        <p v-if="loadedOnce && !nodes.length" class="muted hint">Узлов пока нет</p>
      </nav>

      <footer>
        <button @click="view = { kind: 'add' }">+ Добавить узел</button>
        <div class="row">
          <button class="grow" @click="view = { kind: 'settings' }">Настройки</button>
          <button
            title="Обновить статусы"
            aria-label="Обновить статусы"
            :aria-busy="probing.size > 0"
            @click="refresh"
          >
            ↻
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
        <p v-if="loadedOnce && !nodes.length" class="muted">
          Добавь первый узел: поставь на нём ferry (node/install.ps1) и нажми «Добавить узел».
        </p>
        <div class="cards">
          <button v-for="node in nodes" :key="node.name" class="card tile" @click="open(node.name)">
            <div class="row">
              <span class="dot" :class="tone(node)"></span>
              <strong>{{ node.name }}</strong>
              <span class="muted">{{ node.addr }}</span>
              <span v-if="node.status?.latency_ms != null" class="muted latency">
                {{ node.status.latency_ms }} мс
              </span>
            </div>
            <p class="summary">
              <template v-if="node.paused">пауза на хабе</template>
              <template v-else-if="node.status">{{ node.status.summary }}</template>
              <span v-else class="skeleton"></span>
            </p>
            <ul class="warnings">
              <li v-for="w in node.warnings ?? []" :key="w" class="warning">⚠ {{ w }}</li>
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
  grid-template-columns: 230px minmax(0, 1fr);
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

/* Колонкой — чтобы страница узла могла занять всю высоту (снимок экрана тянется под окно). */
main {
  display: flex;
  flex-direction: column;
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

/* Место под ответ узла занято заранее: две строки статуса и строка предупреждений —
   карточки не растут, когда узел ответит. */
.summary {
  margin: 0;
  min-height: 2lh;
  color: var(--muted);
}

.warnings {
  margin: 0;
  padding: 0;
  min-height: 1lh;
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

@media (max-width: 960px) {
  .app {
    grid-template-columns: 200px minmax(0, 1fr);
  }

  main {
    padding: 18px 20px;
  }
}
</style>
