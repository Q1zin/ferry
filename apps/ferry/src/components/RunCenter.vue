<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { dashboard, desktop, downloadReport, type RunList, type RunSummary, type RunReport } from '../api'
const emit = defineEmits<{ say: [text: string, error?: boolean] }>()
const rows = ref<RunSummary[]>([]), selected = ref<RunReport | null>(null)
const search = ref(''), state = ref(''), cursor = ref<string | null>(null)
const busy = ref(''), error = ref(''), warnings = ref<string[]>([]), actions = ref(desktop)
const confirmCancel = ref(false), expandedId = ref<string | null>(null), detailError = ref('')
const warningPage = ref(0), legacyCount = ref(0)
const warningPageSize = 25
const warningPages = computed(() => Math.ceil(warnings.value.length / warningPageSize))
const visibleWarnings = computed(() => warnings.value.slice(warningPage.value * warningPageSize, (warningPage.value + 1) * warningPageSize))
const baseline = ref(''), comparison = ref<Record<string, unknown> | null>(null)
let stopped = false, timer = 0, detailGeneration = 0
const labels: Record<string, string> = { succeeded: 'Успешно', failed: 'Ошибка', running: 'В работе', pending: 'В очереди', interrupted: 'Прерван', cancelling: 'Отмена', cancelled: 'Отменён', unavailable: 'Недоступен' }
const label = (value: string) => labels[value] ?? value
const visible = computed(() => rows.value.filter(r => (!state.value || r.state === state.value) && `${r.id} ${r.profile} ${r.devices.map(d=>d.device).join(' ')}`.toLowerCase().includes(search.value.toLowerCase())))
const totals = computed(() => ({ active: rows.value.filter(r => r.worker_active).length, passed: rows.value.filter(r => r.state === 'succeeded').length, failed: rows.value.filter(r => r.state === 'failed').length }))
const histogram = computed(() => visible.value.filter(r=>r.duration_seconds != null).slice(0, 24).reverse())
const maxDuration = computed(() => Math.max(1,...histogram.value.map(r=>r.duration_seconds ?? 0)))
const date = (n?: number) => n ? new Date(n * 1000).toLocaleString('ru-RU', {month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}) : '—'
const duration = (n?: number | null) => n == null ? '—' : n < 60 ? `${n} с` : `${Math.floor(n / 60)} м ${n % 60} с`
const bytes = (n?: number) => n == null ? '' : n < 1024 ? `${n} Б` : n < 1048576 ? `${(n / 1024).toFixed(1)} КиБ` : `${(n / 1048576).toFixed(1)} МиБ`
const json = (value: unknown) => JSON.stringify(value, null, 2)
async function refresh(more = false, quiet = false) {
  if (!quiet) busy.value = 'list'
  try {
    const result = await dashboard<RunList>({ action: 'runs', ...(more && cursor.value ? {cursor:cursor.value} : {}) })
    if (stopped) return
    rows.value = more ? [...rows.value,...result.checks.filter(r=>!rows.value.some(x=>x.id===r.id))] : quiet ? [...result.checks,...rows.value.filter(r=>!result.checks.some(x=>x.id===r.id))] : result.checks
    if (!quiet || !cursor.value) cursor.value = result.next_cursor
    warnings.value = [...result.errors, ...(result.scan_truncated ? ['История ограничена первыми 10 000 каталогами; часть старых прогонов может отсутствовать.'] : [])]
    warningPage.value = Math.min(warningPage.value, Math.max(0, warningPages.value - 1))
    legacyCount.value = result.legacy_count ?? 0
    error.value = ''
  } catch (e) { error.value = String(e) }
  finally { if (!quiet) busy.value = '' }
}
function closeDetail(restoreFocus = false) {
  const id = expandedId.value
  expandedId.value = null
  selected.value = null
  detailError.value = ''
  confirmCancel.value = false
  comparison.value = null
  baseline.value = ''
  detailGeneration++
  if (busy.value === id) busy.value = ''
  if (restoreFocus && id) document.getElementById(`run-trigger-${id}`)?.focus()
}
function toggle(id: string) {
  if (expandedId.value === id) closeDetail()
  else open(id)
}
async function open(id: string, quiet = false) {
  if (quiet && expandedId.value !== id) return
  const generation = ++detailGeneration
  if (!quiet) {
    expandedId.value = id
    selected.value = null
    confirmCancel.value = false
    busy.value = id
    comparison.value = null
    baseline.value = ''
  }
  detailError.value = ''
  try {
    const report = await dashboard<RunReport>({action:'run',id})
    if (report.id !== id) throw new Error('Получен результат другого прогона. Обнови данные.')
    if (!stopped && generation === detailGeneration && expandedId.value === id) selected.value = report
  } catch (e) {
    if (!stopped && generation === detailGeneration && expandedId.value === id) detailError.value = String(e)
  } finally {
    if (!quiet && generation === detailGeneration && busy.value === id) busy.value = ''
  }
}
async function command(action: 'resume' | 'cancel') {
  const id = selected.value?.id
  if (!id) return
  confirmCancel.value = false
  busy.value = action
  try { await dashboard({action,id}); await refresh(); await open(id,true) }
  catch(e) { emit('say',String(e),true) }
  finally { busy.value = '' }
}
async function exportRun() {
  if (!selected.value) return
  busy.value = 'export'
  try {
    const result = await downloadReport(selected.value.id)
    emit('say',`${result.complete ? 'Отчёт сохранён' : 'Сохранён неполный отчёт'}${result.archive_file ? `: ${result.archive_file}` : ''}${result.errors.length ? `\n${result.errors.join('\n')}` : ''}`,!result.complete)
  } catch (e) { emit('say',String(e),true) }
  finally { busy.value = '' }
}
async function compare() {
  if (!selected.value || !baseline.value) return
  busy.value = 'compare'
  const id = selected.value.id, base = baseline.value
  try {
    const result = await dashboard<Record<string, unknown>>({action:'compare',id:base,value:id})
    if (expandedId.value === id && baseline.value === base) comparison.value = result
  }
  catch(e) { emit('say',String(e),true) }
  finally { busy.value = '' }
}
async function poll() {
  if (stopped) return
  if (!document.hidden && !busy.value) {
    if (selected.value?.worker_active) await open(selected.value.id,true)
    await refresh(false,true)
  }
  if (!stopped) timer = window.setTimeout(poll,10000)
}
onMounted(async () => {
  dashboard<{read_only:boolean}>({action:'about'}).then(v=>actions.value=!v.read_only).catch(()=>{})
  await refresh(); if (!stopped) timer=window.setTimeout(poll,10000)
})
onUnmounted(()=>{stopped=true;detailGeneration++;clearTimeout(timer)})
</script>
<template>
  <section class="run-center">
    <div class="page-title"><div><p class="eyebrow">Результат работы агента</p><h1>Прогоны</h1></div><button :disabled="!!busy" @click="refresh()">Обновить</button></div>
    <p class="muted">Проверки на этом пульте: этапы, тесты и сохранённые доказательства.</p>
    <div class="metrics"><div><strong>{{ rows.length }}</strong><span>в истории</span></div><div><strong>{{ totals.active }}</strong><span>в работе</span></div><div><strong>{{ totals.passed }}</strong><span>успешно</span></div><div><strong>{{ totals.failed }}</strong><span>с ошибкой</span></div></div>
    <p v-if="error" class="warning" role="alert">{{ error }}</p>
    <details v-if="warnings.length" class="history-warnings card">
      <summary><span><strong>Не удалось прочитать часть истории · {{ warnings.length }}</strong><span>Доступные прогоны показаны ниже. Подробности</span></span></summary>
      <ul class="history-warning-list"><li v-for="(warning,index) in visibleWarnings" :key="warningPage * warningPageSize + index">{{ warning }}</li></ul>
      <div v-if="warningPages > 1" class="row warning-pagination"><button :disabled="warningPage === 0" @click="warningPage--">Предыдущие замечания</button><span class="muted">{{ warningPage * warningPageSize + 1 }}–{{ Math.min((warningPage + 1) * warningPageSize, warnings.length) }} из {{ warnings.length }}</span><button :disabled="warningPage + 1 >= warningPages" @click="warningPage++">Следующие замечания</button></div>
    </details>
    <p v-if="legacyCount" class="muted legacy-history">Старые отчёты: {{ legacyCount }}. Сохранены в <code>~/.ferry/checks</code>; здесь показаны прогоны нового формата.</p>
    <div class="run-filters"><input v-model="search" aria-label="Поиск прогонов" placeholder="Профиль, устройство или ID" /><select v-model="state" aria-label="Состояние прогона"><option value="">Все состояния</option><option v-for="(text,key) in labels" :key="key" :value="key">{{ text }}</option></select></div>
    <figure v-if="histogram.length" class="duration-chart card"><figcaption>Длительность последних {{ histogram.length }} завершённых прогонов <span class="muted">· до {{ duration(maxDuration) }}</span></figcaption><div class="bars"><button v-for="r in histogram" :key="r.id" :title="`${r.id}: ${duration(r.duration_seconds)}`" :aria-label="`${r.id}: ${duration(r.duration_seconds)}`" :class="r.state" :style="{height:`${Math.max(8,(r.duration_seconds ?? 0)/maxDuration*100)}%`}" @click="open(r.id)"></button></div></figure>
    <p v-if="!rows.length && !busy && !error" class="empty card">Проверок пока нет. Подключи агента и запусти первую безопасную проверку — результат появится здесь.</p>
    <p v-else-if="!visible.length && rows.length" class="muted">По этому фильтру прогонов нет.</p>
    <div class="run-list">
      <div v-for="r in visible" :key="r.id" class="run-entry">
        <button :id="`run-trigger-${r.id}`" class="card run-row" :class="{chosen:expandedId===r.id}" :aria-expanded="expandedId===r.id" :aria-controls="`run-details-${r.id}`" @click="toggle(r.id)">
          <div><strong>{{ r.profile }}</strong><span class="state-pill" :class="r.state">{{ label(r.state) }}</span><code>{{ r.id }}</code><span class="run-chevron" aria-hidden="true">{{ expandedId===r.id ? '▾' : '▸' }}</span></div>
          <div class="muted run-meta"><span>{{ date(r.created) }}</span><span>{{ r.devices.map(d=>d.device).join(', ') }}</span><span>{{ r.succeeded }}/{{ r.total }} устройств · {{ r.summary.tests }} тестов · {{ duration(r.duration_seconds) }}</span></div>
        </button>
        <article v-if="expandedId===r.id" :id="`run-details-${r.id}`" class="run-detail card" aria-label="Результаты прогона" :aria-busy="busy===r.id">
          <div v-if="detailError" class="warning" role="alert"><p>{{ detailError }}</p><button :disabled="!!busy" @click="open(r.id)">Повторить загрузку</button></div>
          <p v-if="!selected && !detailError" class="muted" role="status">Загружаю результаты…</p>
          <template v-if="selected && selected.id===r.id">
      <div class="page-title"><div><p class="eyebrow">{{ selected.id }}</p><h2>{{ selected.profile }} <span class="state-pill" :class="selected.state">{{ label(selected.state) }}</span></h2></div><button aria-label="Закрыть результаты" @click="closeDetail(true)">×</button></div>
      <div class="row wrap"><button :disabled="!!busy" @click="exportRun">{{ busy==='export' ? 'Собираю отчёт…' : 'Скачать отчёт ZIP' }}</button><button v-if="actions && selected.state==='interrupted'" :disabled="!!busy" @click="command('resume')">Продолжить</button><button v-if="actions && ['running','interrupted','pending'].includes(selected.state)" class="danger" :disabled="!!busy" @click="confirmCancel=true">Отменить прогон</button><span class="muted">{{ duration(selected.duration_seconds) }}</span></div>
      <div v-if="confirmCancel" class="card warning" role="alert"><p>Остановить собственные задачи прогона {{ selected.id }}?</p><div class="row"><button class="danger" :disabled="!!busy" @click="command('cancel')">Да, отменить</button><button @click="confirmCancel=false">Оставить</button></div></div>
      <p v-if="!actions" class="muted">Панель открыта для просмотра. Управление доступно при запуске сервера с --allow-actions.</p>
      <section v-for="target in selected.results" :key="target.device" class="target-result"><h3>{{ target.device }} <span class="state-pill" :class="target.state">{{ label(target.state) }}</span></h3><p v-if="target.cleanup_pending" class="warning">Очистка ресурсов ещё не подтверждена.</p><p v-for="e in target.errors" :key="e" class="warning">{{ e }}</p>
        <ol class="phases"><li v-for="phase in target.phases" :key="phase.name+phase.job"><div class="row wrap"><strong>{{ phase.name }}</strong><span class="muted">{{ date(phase.started) }} · {{ duration(phase.finished_seen ? phase.finished_seen-phase.started : null) }}</span><code v-if="phase.run_id">{{ phase.run_id }}</code></div><details v-if="phase.result"><summary>Результат этапа</summary><pre>{{ json(phase.result) }}</pre></details></li></ol>
        <div v-if="target.tests" class="test-totals"><strong>{{ target.tests.tests }} тестов</strong><span>{{ target.tests.passed }} пройдено</span><span>{{ target.tests.failed }} падений</span><span>{{ target.tests.errors }} ошибок</span><span>{{ target.tests.skipped }} пропущено</span></div><p v-for="w in target.tests?.warnings" :key="w" class="warning">{{ w }}</p>
        <p v-if="target.test_cases" class="muted">Индекс testcase: {{ target.test_cases.count ?? 0 }} · {{ target.test_cases.complete ? 'полный' : 'неполный' }}<template v-if="target.test_cases.error"> · {{ target.test_cases.error }}</template>. Исходный JUnit — в отчёте.</p>
        <p v-if="target.test_cases?.preview_error" class="warning">{{ target.test_cases.preview_error }}</p>
        <details v-if="target.test_cases?.preview"><summary>Тесты · {{ target.test_cases.preview.cases.length }} в предпросмотре</summary><ul class="test-cases"><li v-for="(test,index) in target.test_cases.preview.cases" :key="index"><span class="state-pill" :class="test.state==='passed'?'succeeded':test.state">{{ test.state }}</span><strong>{{ test.name }}</strong><span class="muted">{{ test.class }} · {{ test.suite.join(' / ') }}</span><pre v-if="test.message || test.detail">{{ test.message }}{{ test.detail ? '\n'+test.detail : '' }}</pre></li></ul><p v-if="target.test_cases.preview.more" class="muted">Ещё {{ target.test_cases.preview.more }} тестов доступны в ZIP-отчёте.</p></details>
        <ul v-if="target.artifacts?.length" class="artifacts"><li v-for="a in target.artifacts" :key="a.id"><span>{{ a.name ?? a.path ?? a.id }}</span><span class="muted">{{ bytes(a.bytes) }}</span></li></ul><p v-else class="muted">Артефакты не зарегистрированы.</p>
        <details v-if="target.source"><summary>Исходники прогона</summary><pre>{{ json(target.source) }}</pre></details>
      </section>
      <div class="compare-box"><h3>Сравнить тесты</h3><div class="row wrap"><select v-model="baseline" aria-label="Базовый прогон"><option value="">Выбери базовый прогон</option><option v-for="r in rows.filter(r=>r.id!==selected?.id && ['succeeded','failed'].includes(r.state))" :key="r.id" :value="r.id">{{ r.profile }} · {{ r.id }}</option></select><button :disabled="!!busy || !baseline" @click="compare">Сравнить</button></div><template v-if="comparison"><p :class="comparison.comparable ? 'muted':'warning'">{{ comparison.comparable ? 'Сопоставимые testcase' : 'Прогоны несопоставимы' }}</p><pre>{{ json(comparison) }}</pre></template></div>
          </template>
        </article>
      </div>
    </div>
    <button v-if="cursor" :disabled="!!busy" @click="refresh(true)">Показать ещё</button>
  </section>
</template>
