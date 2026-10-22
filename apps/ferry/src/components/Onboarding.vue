<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, dashboard, desktop, type Snapshot, type SetupStatus } from '../api'
const props = defineProps<{ snap: Snapshot | null }>()
const emit = defineEmits<{say:[text:string,error?:boolean];pair:[];permissions:[];runs:[]}>()
const status = ref<SetupStatus | null>(null), busy = ref(''), error = ref(''), device = ref('local')
const result = ref<string | null>(null), cliInstalled = ref(false)
const devices = computed(()=>[{value:'local',name:props.snap?.me.name ?? 'Этот компьютер'},...(props.snap?.peers.filter(p=>p.online && p.status?.mode!=='paused' && p.status?.role!=='console').map(p=>({value:p.name,name:p.name})) ?? [])])
async function refresh(){try{const [setup,cli]=await Promise.all([dashboard<SetupStatus>({action:'setup_status'}),api.cli()]);status.value=setup;cliInstalled.value=!!cli.installed;error.value=''}catch(e){error.value=String(e)}}
async function install(agent: 'claude' | 'codex') {
  busy.value=agent
  try {const answer=await dashboard<Record<string,unknown>>({action:'setup_install',value:agent});result.value=JSON.stringify(answer,null,2);await refresh();emit('say',answer.warning ? String(answer.warning) : 'Интеграция настроена. Перезапусти сессию агента, чтобы он увидел ferry.',!!answer.warning)}
  catch(e){emit('say',String(e),true)}finally{busy.value=''}
}
async function cli(){busy.value='cli';try{emit('say',await api.installCli());await refresh()}catch(e){emit('say',String(e),true)}finally{busy.value=''}}
async function smoke(){busy.value='smoke';try{const value=await dashboard<Record<string,unknown>>({action:'setup_smoke',value:device.value});result.value=JSON.stringify(value,null,2);emit('say','Проверка запущена. Результат доступен в центре прогонов.')}catch(e){emit('say',String(e),true)}finally{busy.value=''}}
onMounted(()=>{if(desktop)refresh()})
</script>
<template>
  <section class="onboarding"><p class="eyebrow">От установки к первому результату</p><h1>Подключить агента</h1><p class="muted">ferry даёт агенту доступ к связанным компьютерам и сохраняет результаты проверок. Настрой интеграцию один раз, затем работай из привычной сессии.</p>
    <p v-if="error" class="warning">{{ error }}</p>
    <template v-if="desktop">
      <article class="card onboarding-step"><span class="step-number">1</span><div><h2>Команда и устройства</h2><p>{{ cliInstalled ? 'Команда ferry доступна.' : 'Добавь ferry в PATH, чтобы агент мог вызывать его из терминала.' }}</p><div class="row wrap"><button v-if="!cliInstalled" :disabled="!!busy" @click="cli">Добавить ferry в PATH</button><button @click="emit('pair')">Связать устройство</button><button v-if="snap?.me.os==='macos'" @click="emit('permissions')">Проверить разрешения</button></div><p class="muted">Режим и разрешения каждого устройства по-прежнему определяют, что агент может делать.</p></div></article>
      <article class="card onboarding-step"><span class="step-number">2</span><div><h2>Интеграция с агентом</h2><p class="muted">Установит skill и добавит MCP-сервер ferry. После установки нужна новая сессия агента.</p><div class="agent-options"><div v-for="agent in status?.agents" :key="agent.id" class="agent-option"><h3>{{ agent.id==='claude' ? 'Claude Code' : 'Codex' }}</h3><p class="muted">{{ agent.available ? 'CLI найден' : 'Сначала установи CLI агента' }} · skill {{ agent.skill_installed ? 'готов' : 'не установлен' }} · MCP {{ agent.mcp_configured ? 'готов' : 'не настроен' }}</p><button :disabled="!!busy || !agent.available || agent.config_conflict || !!agent.error" @click="install(agent.id)">{{ busy===agent.id ? 'Настраиваю…' : agent.skill_installed && agent.mcp_configured ? 'Проверить настройку' : 'Настроить' }}</button><p v-if="agent.error || agent.config_conflict" class="warning">{{ agent.error ?? 'Имя MCP ferry занято другой конфигурацией. Она сохранена; проверь настройки агента.' }}</p><code>{{ agent.command }}</code></div></div><p v-if="!status && !error" class="muted">Проверяю установленные агенты…</p></div></article>
      <article class="card onboarding-step"><span class="step-number">3</span><div><h2>Первая проверка</h2><p>Короткая фоновая команда проверит запуск задачи и доставку результата. Без ввода, захвата экрана и изменений проекта.</p><div class="row wrap"><select v-model="device" aria-label="Устройство первой проверки"><option v-for="d in devices" :key="d.value" :value="d.value">{{ d.name }}</option></select><button class="primary" :disabled="!!busy || !snap" @click="smoke">{{ busy==='smoke' ? 'Запускаю…' : 'Запустить проверку' }}</button><button @click="emit('runs')">Открыть прогоны</button></div></div></article>
    </template>
    <article v-else class="card"><h2>Настройка на компьютере с ferry</h2><p>Открой приложение ferry и раздел «Подключить агента» или выполни команды в своём терминале.</p><pre>ferry setup status
ferry setup install claude
# или
ferry setup install codex
ferry setup smoke local</pre><p class="muted">Веб-панель показывает результаты; установка интеграции выполняется на самом компьютере.</p></article>
    <details v-if="result"><summary>Результат настройки</summary><pre>{{ result }}</pre></details>
    <article class="card agent-prompt"><h3>Попроси агента</h3><blockquote>«Используй ferry: проверь доступные устройства, запусти проверку на свободном компьютере и покажи результат с артефактами».</blockquote><p class="muted">Для проекта: <code>ferry init --template smoke</code>. Также доступны шаблоны tauri, electron и web.</p></article>
  </section>
</template>
