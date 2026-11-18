<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, blocked, supportsProjectFeature, type PeerView, type ProjectInspection, type ProjectReceipt, type ProjectShell } from '../api'
import ProjectStorage from './ProjectStorage.vue'
import ProjectTransfer from './ProjectTransfer.vue'

const props = defineProps<{ peers: PeerView[]; incoming?: boolean }>()
const tab = ref<'outgoing' | 'incoming' | 'storage'>(props.incoming ? 'incoming' : 'outgoing')
const path = ref('')
const deviceId = ref('')
const command = ref('')
const shell = ref<ProjectShell>('default')
const source = ref<{ path: string; inspected: ProjectInspection } | null>(null)
const busy = ref<'inspect' | 'begin' | null>(null)
const error = ref('')
const history = ref<ProjectReceipt[]>([])
const historyError = ref('')
const loading = ref(false)
const loaded = ref(false)
let refreshTimer = 0

function reason(peer: PeerView) {
  return blocked(peer) || (supportsProjectFeature(peer.status, 31) ? '' : 'Нужна Ferry 0.1.31 или новее.')
}

const inspected = computed(() => source.value?.path === path.value.trim() ? source.value?.inspected : null)
const target = computed(() => props.peers.find(peer => peer.id === deviceId.value))
const targetError = computed(() => target.value ? reason(target.value) : '')
const launch = computed(() => command.value.trim())
const visible = computed(() => history.value.filter(receipt => receipt.direction === tab.value))
const beginDisabled = computed(() => !!busy.value || !inspected.value || !target.value || !!targetError.value)
const receiptKey = (receipt: ProjectReceipt) => receipt.direction + ':' + receipt.id

watch(() => props.incoming, incoming => { tab.value = incoming ? 'incoming' : 'outgoing' })
watch(() => target.value?.status?.os, () => { shell.value = 'default' })
watch(path, () => { source.value = null; error.value = '' })

async function inspect() {
  const requested = path.value.trim()
  if (busy.value || !requested) return
  busy.value = 'inspect'
  error.value = ''
  source.value = null
  try {
    source.value = { path: requested, inspected: await api.projectTransfer('inspect', { path: requested }) }
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = null
  }
}

async function begin() {
  if (beginDisabled.value) return
  busy.value = 'begin'
  error.value = ''
  try {
    const receipt = await api.projectTransfer('begin', {
      path: path.value.trim(),
      device: deviceId.value,
      ...(launch.value ? { command: launch.value } : {}),
      shell: launch.value ? shell.value : 'default',
    })
    history.value = [receipt, ...history.value.filter(item => receiptKey(item) !== receiptKey(receipt))]
  } catch (e) {
    error.value = String(e)
  } finally {
    busy.value = null
    void refresh()
  }
}

async function refresh() {
  if (loading.value) return
  loading.value = true
  try {
    const { transfers } = await api.projectTransfer('history', {})
    history.value = transfers
    loaded.value = true
    historyError.value = ''
  } catch (e) {
    historyError.value = String(e)
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  void refresh()
  refreshTimer = window.setInterval(() => {
    if (tab.value !== 'storage' && document.visibilityState === 'visible') void refresh()
  }, 3000)
})
onUnmounted(() => window.clearInterval(refreshTimer))
</script>

<template>
  <section class="projects">
    <header class="page-title"><h1>Проекты</h1></header>
    <nav class="row" aria-label="Разделы проектов">
      <button :aria-pressed="tab === 'outgoing'" @click="tab = 'outgoing'">Отправка</button>
      <button :aria-pressed="tab === 'incoming'" @click="tab = 'incoming'">Принято</button>
      <button :aria-pressed="tab === 'storage'" @click="tab = 'storage'">Место на устройствах</button>
    </nav>

    <div v-if="tab === 'outgoing'" class="card project-form">
      <h2>Отправить рабочую копию</h2>
      <p class="muted">Ferry передаст снимок локального рабочего дерева с незакоммиченными правками. Git push не нужен.</p>
      <div class="row">
        <label class="project-path">Локальная папка
          <input v-model="path" :disabled="!!busy" placeholder="/путь/к/репозиторию" @keydown.enter="inspect" />
        </label>
        <button :disabled="!!busy || !path.trim()" :aria-busy="busy === 'inspect'" @click="inspect">Проверить папку</button>
      </div>
      <template v-if="inspected">
        <div class="project-source">
          <strong>{{ inspected.project }}</strong>
          <code>{{ inspected.root }}</code>
          <span v-if="inspected.prefix">Рабочий подкаталог <code>{{ inspected.prefix }}</code></span>
          <p v-if="inspected.extras.length">Дополнительные файлы из профиля · <code>{{ inspected.extras.join(', ') }}</code></p>
          <p v-if="inspected.caches.length" class="muted">Кандидаты для очистки · <code>{{ inspected.caches.join(', ') }}</code></p>
        </div>
        <label>Устройство назначения
          <select v-model="deviceId" :disabled="!!busy">
            <option value="" disabled>Выбери устройство</option>
            <option v-for="peer in peers" :key="peer.id" :value="peer.id" :disabled="!!reason(peer)">
              {{ peer.name }}{{ reason(peer) ? ' · ' + reason(peer) : '' }}
            </option>
          </select>
        </label>
        <p v-if="targetError" class="warning">{{ targetError }}</p>
        <p v-else-if="!peers.length" class="muted">Чтобы отправить проект, свяжи второе устройство.</p>
        <label>Команда после синхронизации
          <input v-model="command" :disabled="!!busy" placeholder="cargo test (необязательно)" />
        </label>
        <div class="row form-actions">
          <label v-if="launch">Оболочка
            <select v-model="shell" :disabled="!!busy">
              <option value="default">По умолчанию</option>
              <option value="bash">Bash</option>
              <option v-if="target?.status?.os === 'windows'" value="powershell">PowerShell</option>
              <option v-if="target?.status?.os === 'windows'" value="cmd">cmd</option>
            </select>
          </label>
          <button class="primary" :disabled="beginDisabled" :aria-busy="busy === 'begin'" @click="begin">
            {{ launch ? 'Синхронизировать и запустить' : 'Синхронизировать' }}
          </button>
        </div>
      </template>
      <p v-if="error" class="warning" role="alert">{{ error }}</p>
    </div>

    <ProjectStorage v-if="tab === 'storage'" :peers="peers" />
    <section v-else class="transfer-history">
      <div class="page-title">
        <h2>{{ tab === 'incoming' ? 'Входящие передачи' : 'Исходящие передачи' }}</h2>
        <button :disabled="loading" @click="refresh">Обновить передачи</button>
      </div>
      <p v-if="historyError" class="warning" role="alert">{{ historyError }}</p>
      <p v-else-if="!loaded" class="muted" role="status">Загружаю историю…</p>
      <p v-else-if="!visible.length" class="muted">Передач пока нет.</p>
      <ProjectTransfer
        v-for="receipt in visible"
        :key="receiptKey(receipt)"
        :receipt="receipt"
        :peer="peers.find(peer => peer.id === receipt.peer_id)?.name ?? receipt.peer_id"
      />
    </section>
  </section>
</template>

<style scoped>
.projects, .project-form, .transfer-history { display: grid; gap: 12px; min-width: 0; }
.projects { max-width: 960px; }
.projects p { margin: 0; }
.projects nav button[aria-pressed="true"] { color: var(--coral); border-color: var(--coral); }
.project-form .row { align-items: end; }
.project-form label { display: grid; gap: 5px; min-width: 0; }
.project-path { flex: 1 1 250px; }
.project-form select option { text-overflow: ellipsis; }
.project-form select { width: 100%; }
.form-actions label { flex: 0 1 200px; }
.project-source { display: grid; gap: 5px; padding: 10px 0; overflow-wrap: anywhere; border-block: 1px solid var(--line); }
</style>
