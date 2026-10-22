<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import {
  api,
  desktop,
  modeLabel,
  onOpenPairing,
  onState,
  osLabel,
  path,
  roleLabel,
  tone,
  type Permissions,
  type Snapshot,
} from './api'
import logo from '../src-tauri/icons/128x128.png'
import Device from './components/Device.vue'
import Pairing from './components/Pairing.vue'
import PermissionsView from './components/Permissions.vue'
import Settings from './components/Settings.vue'
import HumanQuestions from './components/HumanQuestions.vue'
import RunCenter from './components/RunCenter.vue'
import Onboarding from './components/Onboarding.vue'

type View =
  | { kind: 'home' }
  | { kind: 'device'; id: string | null }
  | { kind: 'pairing' }
  | { kind: 'settings' }
  | { kind: 'permissions' }
  | { kind: 'runs' }
  | { kind: 'onboarding' }

const snap = ref<Snapshot | null>(null)
const view = ref<View>({ kind: 'home' })
const notice = ref<{ text: string; error: boolean } | null>(null)
/// Версия новее этой: проверяется тихо при запуске и в настройках.
const update = ref<string | null>(null)
/// Доступы macOS; null — не мак. Чего-то не хватает — страница разрешений открывается сама.
const perms = ref<Permissions | null>(null)
const missing = computed(() => !!perms.value && (!perms.value.screen || !perms.value.input))

const version = computed(() => snap.value?.me.version ?? '')

/// Страница устройства: `id` пуст — это устройство.
const device = computed(() => {
  const v = view.value
  if (v.kind !== 'device' || !snap.value) return undefined
  if (v.id === null) return { id: null, name: snap.value.me.name, online: true, status: snap.value.me, path: '' }
  const p = snap.value.peers.find((p) => p.id === v.id)
  return p && { id: p.id, name: p.name, online: p.online, status: p.status, path: path(p) }
})

let noticeTimer = 0
function say(text: string, error = false) {
  if (!text.trim()) return
  notice.value = { text: text.trim(), error }
  clearTimeout(noticeTimer)
  noticeTimer = window.setTimeout(() => (notice.value = null), error ? 10000 : 5000)
}

const unlisten: (() => void)[] = []
let pollTimer = 0, disposed = false
async function refreshWeb() {
  try { snap.value = await api.state() } catch (e) { say(String(e), true) }
  if (!disposed) pollTimer = window.setTimeout(refreshWeb, 15000)
}
onMounted(async () => {
  unlisten.push(await onState((s) => (snap.value = s)))
  unlisten.push(await onOpenPairing(() => (view.value = { kind: 'pairing' })))
  try {
    snap.value = await api.state()
  } catch (e) {
    say(String(e), true)
  }
  if (!desktop) { pollTimer = window.setTimeout(refreshWeb, 15000); return }
  perms.value = await api.permissions().catch(() => null)
  if (missing.value) view.value = { kind: 'permissions' }
  // «Связать устройство…» из трея, когда окна ещё не было (main_window в src-tauri).
  if ('FERRY_PAIRING' in window) view.value = { kind: 'pairing' }
  // Нет сети или релиза — просто не показываем.
  api.checkUpdate().then(
    (v) => (update.value = v),
    () => {},
  )
})
onUnmounted(() => { disposed = true; clearTimeout(pollTimer); clearTimeout(noticeTimer); unlisten.forEach((f) => f()) })
</script>

<template>
  <div class="app">
    <HumanQuestions v-if="desktop" @say="say" />
    <aside>
      <button class="brand" @click="view = { kind: 'home' }">
        <img :src="logo" alt="" />
        <span>
          <strong>ferry</strong>
          <span class="muted">{{ desktop ? 'агент · устройства' : 'веб-панель' }}</span>
        </span>
      </button>

      <div class="primary-nav">
        <button :class="{active:view.kind==='home'}" @click="view={kind:'home'}">Устройства</button>
        <button :class="{active:view.kind==='runs'}" @click="view={kind:'runs'}">Прогоны</button>
        <button :class="{active:view.kind==='onboarding'}" @click="view={kind:'onboarding'}">Подключить агента</button>
      </div>
      <nav v-if="snap && desktop">
        <p class="section muted">Это устройство</p>
        <button
          class="node-link"
          :class="{ active: view.kind === 'device' && view.id === null }"
          @click="desktop && (view = { kind: 'device', id: null })"
        >
          <span class="dot" :class="tone(snap.me, true, version)"></span>
          <span class="name">{{ snap.me.name }}</span>
        </button>

        <p class="section muted">Связанные</p>
        <button
          v-for="p in snap.peers"
          :key="p.id"
          class="node-link"
          :class="{ active: view.kind === 'device' && view.id === p.id }"
          @click="desktop && (view = { kind: 'device', id: p.id })"
        >
          <span class="dot" :class="tone(p.status, p.online, version)"></span>
          <span class="name">{{ p.name }}</span>
        </button>
        <p v-if="!snap.peers.length" class="muted hint">Пока ни одного</p>
      </nav>

      <footer v-if="desktop">
        <button @click="view = { kind: 'pairing' }">+ Связать устройство</button>
        <button v-if="perms" @click="view = { kind: 'permissions' }">
          Разрешения<template v-if="missing"> · нужно выдать</template>
        </button>
        <button @click="view = { kind: 'settings' }">
          Настройки<template v-if="update"> · есть {{ update }}</template>
        </button>
      </footer>
    </aside>

    <main>
      <RunCenter v-if="view.kind==='runs'" @say="say" />
      <Onboarding v-else-if="view.kind==='onboarding'" :snap="snap" @say="say" @pair="view={kind:'pairing'}" @permissions="view={kind:'permissions'}" @runs="view={kind:'runs'}" />
      <Device
        v-else-if="desktop && view.kind === 'device' && device"
        :key="device.id ?? 'me'"
        :device="device"
        :version="version"
        :activity="snap?.activity"
        @say="say"
        @gone="view = { kind: 'home' }"
      />
      <Pairing
        v-else-if="view.kind === 'pairing' && snap"
        :snap="snap"
        @say="say"
        @paired="(id: string) => (view = { kind: 'device', id })"
      />
      <PermissionsView
        v-else-if="view.kind === 'permissions'"
        @say="say"
        @changed="(p: Permissions) => (perms = p)"
      />
      <Settings
        v-else-if="view.kind === 'settings' && snap"
        :snap="snap"
        :update="update"
        @say="say"
        @checked="(v: string | null) => (update = v)"
      />
      <section v-else class="home">
        <div class="page-title"><div><p class="eyebrow">Рабочее пространство агента</p><h1>Устройства</h1></div><button @click="view={kind:'runs'}">Результаты проверок →</button></div>
        <p class="muted fleet-intro">Состояние связанных компьютеров и доступность для задач. Режим «работает» сам по себе не означает, что рабочий стол свободен.</p>
        <div v-if="snap" class="metrics fleet-metrics"><div><strong>{{ 1 + snap.peers.filter(p=>p.online).length }}</strong><span>на связи</span></div><div><strong>{{ snap.peers.filter(p=>!p.online).length }}</strong><span>недоступно</span></div><div><strong>{{ snap.peers.filter(p=>p.status?.mode==='no-desktop').length + (snap.me.mode==='no-desktop' ? 1 : 0) }}</strong><span>без рабочего стола</span></div></div>
        <p v-if="!desktop" class="muted">Веб-панель · управление устройствами доступно в приложении ferry.</p>
        <div v-if="!snap" class="card"><p>Нет состояния ferry. Проверь, что приложение запущено.</p><button @click="api.state().then(s=>snap=s).catch(e=>say(String(e),true))">Повторить подключение</button></div>
        <div v-if="snap" class="cards">
          <button class="card tile" @click="desktop && (view = { kind: 'device', id: null })">
            <div class="row">
              <span class="dot" :class="tone(snap.me, true, version)"></span>
              <strong>{{ snap.me.name }}</strong>
              <span class="muted">это устройство</span>
            </div>
            <p class="summary">
              {{ osLabel(snap.me.os) }} · {{ roleLabel(snap.me.role) }} · {{ modeLabel(snap.me.mode) }}
            </p>
          </button>
          <button v-for="p in snap.peers" :key="p.id" class="card tile" @click="desktop && (view = { kind: 'device', id: p.id })">
            <div class="row">
              <span class="dot" :class="tone(p.status, p.online, version)"></span>
              <strong>{{ p.name }}</strong>
              <span class="muted path">{{ path(p) }}</span>
            </div>
            <p class="summary">
              <template v-if="p.status">
                {{ osLabel(p.status.os) }} · {{ roleLabel(p.status.role) }} · {{ modeLabel(p.status.mode) }}
              </template>
              <template v-else>ещё не отвечало с запуска</template>
            </p>
            <p v-if="p.status && p.status.version !== version" class="warning">
              ⚠ ferry {{ p.status.version }}, здесь {{ version }}
            </p>
          </button>
          <button v-if="desktop" class="card tile add" @click="view = { kind: 'pairing' }">
            <strong>+ Связать устройство</strong>
            <p class="summary">Поставь ferry на другой компьютер и введи код с него здесь — или наоборот.</p>
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
  height: 100dvh;
}

aside {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 14px 12px;
  overflow: auto;
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

.section {
  margin: 10px 10px 4px;
  font-size: 12px;
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
  padding: 4px 10px;
}

footer {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

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
  grid-template-columns: repeat(auto-fill, minmax(min(280px, 100%), 1fr));
  gap: 12px;
}

.tile {
  text-align: left;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.tile.add {
  border-style: dashed;
}

.path {
  margin-left: auto;
}

.summary,
.tile .warning {
  margin: 0;
}

.summary {
  color: var(--muted);
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
.primary-nav { display:flex;flex-direction:column;gap:4px; }
.primary-nav button { text-align:left;border-color:transparent;background:transparent; }
.primary-nav button.active { color:var(--coral);background:var(--panel);border-color:var(--line);font-weight:600; }
.fleet-intro { max-width:68em;margin-top:0; }
.fleet-metrics { margin-bottom:20px; }
@media (max-width:640px) {
 .app { grid-template-columns:1fr;grid-template-rows:auto minmax(0,1fr); }
 aside { padding:8px 12px;border-right:0;border-bottom:1px solid var(--line);gap:8px; }
 .brand { display:none; }
 .primary-nav { flex-direction:row;flex-wrap:wrap; }
 nav, footer { display:none; }
 main { padding:16px; }
 .notice { right:12px;left:12px;bottom:12px; }
}
</style>
