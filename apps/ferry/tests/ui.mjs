// Смоук-тест приложения в Chromium (`bun run test:ui` после `bun run build`).
// Мост Tauri подменён: invoke отвечает фикстурами и записывает вызовы, а события агента
// (`state`, `open-pairing`) тест присылает сам — как их шлёт бэкенд. Проверяем отрисовку, что
// уходит в бэкенд, что изменения приходят событиями без опроса и что вёрстка держится в обычном
// и минимальном окне. Скриншоты — в FERRY_UI_SHOTS, если задан.
import assert from 'node:assert/strict'
import { readFileSync, existsSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'
import { chromium } from 'playwright-core'

const HERE = dirname(fileURLToPath(import.meta.url))
const DIST = join(HERE, '../dist')
const SHOTS = process.env.FERRY_UI_SHOTS

const WIN = 'c6a00fcf9e392bd186e03913f4ebd20b4281efb7259bb2b864fb45af9d964e75'
const OLD = '5f2a0a1b9e392bd186e03913f4ebd20b4281efb7259bb2b864fb45af9d96aaaa'
const NEW = '0d9c2e7a1b392bd186e03913f4ebd20b4281efb7259bb2b864fb45af9d96bbbb'

const me = { name: 'macbook', role: 'both', mode: 'active', os: 'macos', version: '0.1.0' }
const win = { name: 'win-10', role: 'node', mode: 'active', os: 'windows', version: '0.1.0' }
const snapshot = {
  id: 'a7f3e6d1c2b392bd186e03913f4ebd20b4281efb7259bb2b864fb45af9d96cccc',
  me,
  peers: [
    { id: WIN, name: 'win-10', online: true, status: win, direct: true, rtt_ms: 6 },
    { id: OLD, name: 'win-11', online: false, status: null, direct: null, rtt_ms: null },
  ],
  lan: [{ id: NEW, name: 'office-pc', pairing: true }],
  pairing: null,
  activity: [
    { at: 1792335600, who: 'win-11', what: 'команда: cargo test', desk: false },
    { at: 1792335660, who: 'win-11', what: 'ввод: click 458 390', desk: true },
  ],
}
// Экран устройства 1366×768 с меткой: по ней видно, что снимок показан.
const screen = new Resvg(
  `<svg xmlns="http://www.w3.org/2000/svg" width="1366" height="768">
    <rect width="1366" height="768" fill="#1d4f91"/><rect x="221" y="125" width="474" height="531" fill="#fff"/></svg>`,
).render().asPng()

const fixtures = {
  questions: [],
  state: snapshot,
  detail: {
    properties: [
      ['система', 'Майкрософт Windows 10 Корпоративная LTSC (17763)'],
      ['процессор', 'Intel(R) Pentium(R) CPU B960 @ 2.20GHz, потоков: 2'],
      ['диск', 'свободно 6,5 из 118,7 ГБ'],
    ],
    warnings: ['на диске 6 ГБ — для сборки с CEF мало'],
  },
  screenshot: `data:image/png;base64,${Buffer.from(screen).toString('base64')}`,
  input: '',
  jobs: [
    { name: 'app', label: 'работает', ago: '2 мин назад', running: true, cmd: './logarithmica.exe --remote-debugging-port=9322' },
    { name: 'build', label: 'завершилась, код 0', ago: '9 мин назад', running: false, cmd: 'cargo build' },
  ],
  job_log: 'Compiling ferry v0.1.0\nFinished in 41s\n',
  start_job: 'ferry: задача tests запущена',
  open_pairing: { code: '4821-0937', ticket: `ferry:${snapshot.id}:4821-0937`, seconds_left: 600 },
  pair: { id: NEW, name: 'office-pc', online: true, status: { ...win, name: 'office-pc' }, direct: true, rtt_ms: 3 },
  autostart: true,
  cli: { path: '/Applications/ferry.app/Contents/MacOS/ferry', installed: null },
  install_cli: 'Команда ferry: /opt/homebrew/bin/ferry',
  version: '0.1.0',
  check_update: '0.1.1',
  update_all: 'win-10: ferry 0.1.0 → 0.1.1: ставлю и перезапускаюсь',
  install_update: 'ferry 0.1.0 → 0.1.1: ставлю и перезапускаюсь',
}

const runBase = { id: 'check-first', profile: 'smoke', created: 1792335600, finished_at: 1792335641, duration_seconds: 41, state: 'succeeded', worker_active: false, total: 1, succeeded: 1, devices: [{device:'win-10',state:'succeeded'}], summary:{tests:3,passed:3,failed:0,errors:0,skipped:0,artifacts:1} }
const report = { ...runBase, results: [{device:'win-10',state:'succeeded',phases:[{name:'test',job:'test',run_id:'run-one',started:1792335600,finished_seen:1792335641,result:{exit_code:0,stdout:'<script>literal output</script>'}}],tests:runBase.summary,artifacts:[{id:'junit-1',name:'junit.xml',bytes:1024}],test_cases:{count:3,complete:true,preview:{cases:[{suite:['smoke'],class:'connection',name:'execution',state:'passed',duration_seconds:0.1}],more:2,complete:true}}}] }
fixtures.dashboard = {
  about: {read_only:false,web:false},
  runs: {checks:[{...runBase,id:'check-second',state:'failed',duration_seconds:58,succeeded:0},runBase],next_cursor:null,errors:[]},
  run: report,
  compare: {comparable:true,regressions:[{identity:'suite:test',before:{state:'passed'},after:{state:'failed'}}],improvements:[],unchanged:2,errors:[]},
  export: {archive_file:'/Downloads/ferry-reports/report.zip',complete:true,errors:[]},
  setup_status: {cli_available:true,app_available:true,agents:[{id:'claude',available:true,skill_installed:false,mcp_configured:false,command:'ferry setup install claude'},{id:'codex',available:true,skill_installed:true,mcp_configured:true,command:'ferry setup install codex'}]},
  setup_install: {installed:true},setup_smoke:{id:'smoke-first',state:'running'},
}

const server = createServer((req, res) => {
  const path = join(DIST, req.url === '/' ? 'index.html' : decodeURIComponent(req.url.split('?')[0]))
  if (!existsSync(path)) return res.writeHead(404).end()
  const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.png': 'image/png' }
  res.writeHead(200, { 'content-type': types[extname(path)] ?? 'application/octet-stream' })
  res.end(readFileSync(path))
})
await new Promise((ok) => server.listen(0, '127.0.0.1', ok))
const url = `http://127.0.0.1:${server.address().port}/`

// Обычное окно — светлая тема, минимальное (tauri.conf.json: minWidth/minHeight) — тёмная.
const [win0] = JSON.parse(readFileSync(join(HERE, '../src-tauri/tauri.conf.json'))).app.windows
const runs = [
  { scheme: 'light', width: win0.width, height: win0.height },
  { scheme: 'dark', width: win0.minWidth, height: win0.minHeight },
]

const browser = await chromium.launch()
const failures = []
try {
  for (const { scheme, width, height } of runs) {
    const run = `${scheme}-${width}x${height}`
    const page = await browser.newPage({ viewport: { width, height }, colorScheme: scheme })
    page.on('pageerror', (e) => failures.push(`${run}: ${e.message}`))
    await page.addInitScript((fx) => {
      window.__calls = []
      const callbacks = {}
      const listeners = {}
      let next = 0
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} }
      window.__TAURI_INTERNALS__ = {
        transformCallback: (cb) => ((callbacks[++next] = cb), next),
        invoke: async (cmd, args = {}) => {
          if (cmd === 'plugin:event|listen') {
            ;(listeners[args.event] ??= []).push(args.handler)
            return next
          }
          window.__calls.push([cmd, args])
          if (cmd === 'dashboard') return fx.dashboard[args.request.action]
          if (cmd === 'start_stream') { window.__stream = args.channel; return 42 }
          return cmd in fx ? fx[cmd] : null
        },
      }
      // Событие агента — как его доставляет Tauri.
      window.__emit = (event, payload) => (listeners[event] ?? []).forEach((id) => callbacks[id]({ event, payload }))
    }, fixtures)
    const calls = () => page.evaluate(() => window.__calls.splice(0))
    const emit = (event, payload) => page.evaluate(([e, p]) => window.__emit(e, p), [event, payload])
    const shot = (name) => SHOTS && page.screenshot({ path: join(SHOTS, `${run}-${name}.png`) })
    const fits = async (where) => {
      const [scroll, client] = await page.locator('main').evaluate((m) => [m.scrollWidth, m.clientWidth])
      assert.ok(scroll <= client, `${run}, ${where}: вбок на ${scroll - client}px`)
    }

    // Главный экран: это устройство, связанные (на связи и нет) и карточка «связать».
    await page.goto(url)
    await page.getByText('напрямую, 6 мс').waitFor()
    assert.equal(await page.locator('.tile').count(), 4)
    await page.getByText('не на связи').first().waitFor()
    await fits('устройства')
    await shot('home')

    // Изменения приходят событием, без опроса: state запрошен один раз, при запуске.
    const quiet = { ...snapshot, peers: [{ ...snapshot.peers[0], status: { ...win, mode: 'no-desktop' } }, snapshot.peers[1]] }
    await emit('state', quiet)
    await page.getByText('Windows · узел · не трогать рабочий стол').waitFor()
    assert.equal((await calls()).filter(([c]) => c === 'state').length, 1, 'state опрашивается')
    await emit('state', snapshot)

    // Вопрос человеку — виден поверх любой страницы, текст не исполняется как HTML.
    const question = { id: 'help-one', from: 'MacBook · agent:test', text: 'Подтвердите вход в тестовое приложение\n<script>throw 1</script>', expires_at: 1893456000, state: 'pending' }
    await emit('human-questions', [question])
    const help = page.getByRole('dialog', { name: 'Агенту нужна ваша помощь' })
    await help.waitFor()
    assert.ok(await help.evaluate((el) => el.matches(':modal') && el.contains(document.activeElement)))
    await page.keyboard.press('Escape')
    assert.ok(await help.isVisible(), 'Escape не должен превращаться в ответ')
    assert.equal(await help.locator('script').count(), 0)
    assert.ok((await help.innerText()).includes('<script>throw 1</script>'))
    const bounds = await help.boundingBox()
    assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= width)
    await shot('human-help')
    await help.getByRole('button', { name: 'Не могу выполнить' }).click()
    await help.waitFor({ state: 'hidden' })
    assert.deepEqual((await calls()).find(([c]) => c === 'answer'), ['answer', { id: 'help-one', answer: 'declined' }])
    await emit('human-questions', [{ ...question, id: 'help-two', text: 'Откройте тестовое окно' }])
    await help.getByRole('button', { name: 'Готово', exact: true }).click()
    await help.waitFor({ state: 'hidden' })
    assert.deepEqual((await calls()).find(([c]) => c === 'answer'), ['answer', { id: 'help-two', answer: 'done' }])
    await emit('human-questions', [question])
    await help.waitFor()
    await emit('human-questions', [])
    await help.waitFor({ state: 'hidden' })

    // Чужое устройство: роль меняется отсюда, режим — только виден.
    await page.locator('nav .node-link', { hasText: 'win-10' }).click()
    await page.getByRole('heading', { name: 'win-10' }).waitFor()
    assert.equal(await page.getByRole('button', { name: 'не трогать рабочий стол' }).count(), 0)
    await page.getByRole('button', { name: 'пульт', exact: true }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'set_role'))
    assert.deepEqual((await calls()).find(([c]) => c === 'set_role'), ['set_role', { id: WIN, role: 'console' }])
    // Свойства — сами, как только устройство на связи.
    await page.getByText('Pentium').waitFor()
    await page.getByText('на диске 6 ГБ — для сборки с CEF мало').waitFor()
    await fits('чужое устройство')
    await shot('peer')

    // «Экран»: клик по снимку уходит на устройство в пикселях его экрана.
    await page.getByRole('button', { name: 'Экран', exact: true }).click()
    const img = page.locator('.frame img')
    await img.waitFor()
    await page.waitForFunction(() => document.querySelector('.frame img')?.naturalWidth > 0)
    const box = await img.boundingBox()
    await calls()
    const [cx, cy] = [Math.round(box.x + box.width * 0.5), Math.round(box.y + box.height * 0.25)]
    let want = [Math.round(((cx - box.x) * 1366) / box.width), Math.round(((cy - box.y) * 768) / box.height)]
    await page.mouse.click(cx, cy)
    await page.waitForTimeout(300)
    assert.equal((await calls()).filter(([c,a])=>c==='control_screen' && a.args?.length).length,0,'просмотр не должен вводить')
    await page.getByRole('button',{name:'Живой экран',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c])=>c==='start_stream'))
    await page.evaluate(()=>window.__stream.onmessage({frame:{jpeg:'',source_width:1366,source_height:768,origin_x:0,origin_y:0}}))
    await page.getByRole('button',{name:'Живой экран',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c,a])=>c==='stop_stream' && a.token===42))
    await page.getByRole('button',{name:'Обновить',exact:true}).click()
    await page.waitForFunction(()=>document.querySelector('.frame img')?.naturalWidth>0)
    await page.getByRole('button',{name:'Управлять',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c,a])=>c==='control_screen' && a.args?.length===0))
    const activeBox=await img.boundingBox()
    want=[Math.floor(((cx-activeBox.x)*1366)/activeBox.width),Math.floor(((cy-activeBox.y)*768)/activeBox.height)]
    await page.mouse.click(cx, cy)
    await page.waitForFunction(() => window.__calls.some(([c,a]) => c === 'control_screen' && a.args?.[0]==='click'))
    const [, click] = (await calls()).find(([c,a]) => c === 'control_screen' && a.args?.[0]==='click')
    assert.equal(click.id, WIN)
    assert.equal(click.args[0], 'click')
    assert.ok(Math.abs(click.args[1] - want[0]) <= 1 && Math.abs(click.args[2] - want[1]) <= 1, `клик ${click.args}, ждали ${want}`)
    await page.getByPlaceholder('Ввести текст на устройстве').fill('Привет')
    await page.getByRole('button', { name: 'Ввести' }).click()
    await page.waitForFunction(() => window.__calls.some(([c, a]) => c === 'control_screen' && a.args?.[0] === 'type'))
    await fits('экран')
    await shot('screen')

    // «Задачи»: список, лог, запуск.
    await page.getByRole('button', { name: 'Задачи', exact: true }).click()
    await page.getByText('logarithmica.exe').waitFor()
    await page.getByRole('listitem').filter({ hasText: 'cargo build' }).getByRole('button', { name: 'Лог' }).click()
    await page.getByText('Finished in 41s').waitFor()
    await page.getByPlaceholder('имя: app').fill('tests')
    await page.getByPlaceholder('команда').fill('bun test')
    await calls()
    await page.getByRole('button', { name: 'Запустить' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'start_job'))
    assert.deepEqual((await calls()).find(([c]) => c === 'start_job'), ['start_job', { id: WIN, name: 'tests', cmd: 'bun test' }])
    await fits('задачи')
    await shot('jobs')
    await page.getByRole('button', { name: 'Обзор', exact: true }).click()

    // Устройство не на связи: управлять нечем, но видно почему — и на вкладках тоже.
    await page.locator('nav .node-link', { hasText: 'win-11' }).click()
    await page.getByText('Не на связи: выключено').waitFor()
    assert.ok(await page.getByRole('button', { name: 'узел', exact: true }).isDisabled())
    await page.getByRole('button', { name: 'Экран', exact: true }).click()
    await page.getByText('Устройство не на связи.').waitFor()
    await page.getByRole('button', { name: 'Обзор', exact: true }).click()
    // Забыть — сразу, без подтверждения: confirm() в окне Tauri на маке не показывается.
    await page.getByRole('button', { name: 'Забыть устройство' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'forget'))
    assert.deepEqual((await calls()).find(([c]) => c === 'forget'), ['forget', { id: OLD }])
    await page.getByRole('heading', { name: 'Устройства' }).waitFor()

    // Это устройство: режим выбирают здесь.
    await page.locator('nav .node-link', { hasText: 'macbook' }).click()
    // Журнал этого устройства: новые сверху, действия на рабочем столе выделены.
    const journal = page.locator('.journal li')
    await journal.first().getByText('ввод: click 458 390').waitFor()
    assert.equal(await journal.count(), 2)
    await fits('журнал')
    await shot('journal')
    await page.getByRole('button', { name: 'не трогать рабочий стол' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'set_mode'))
    assert.deepEqual((await calls()).find(([c]) => c === 'set_mode'), ['set_mode', { mode: 'no-desktop' }])
    await shot('me')

    // Трей «Связать устройство…» открывает связывание; рядом видно, кто ждёт.
    await emit('open-pairing', null)
    await page.getByRole('heading', { name: 'Связать устройство' }).waitFor()
    await page.getByText('office-pc').waitFor()
    // Показать код: окно открывается в агенте, код приходит с событием.
    await page.getByRole('button', { name: 'Показать код' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'open_pairing'))
    await emit('state', { ...snapshot, pairing: fixtures.open_pairing })
    await page.getByText('4821-0937', { exact: true }).waitFor()
    await page.getByText(/Код работает (9:5\d|10:00)/).waitFor()
    await fits('связывание')
    await shot('pairing')
    // Ввести код с другого устройства — и сразу на его страницу.
    await page.getByPlaceholder('1234-5678 или ferry:…').fill('1234-5678')
    await page.getByRole('button', { name: 'Связать', exact: true }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'pair'))
    assert.deepEqual((await calls()).find(([c]) => c === 'pair'), ['pair', { code: '1234-5678' }])
    await emit('state', { ...snapshot, peers: [...snapshot.peers, fixtures.pair], lan: [] })
    await page.getByRole('heading', { name: 'office-pc' }).waitFor()

    // Настройки: автозапуск.
    await page.getByRole('button', { name: 'Настройки' }).click()
    const autostart = page.getByRole('checkbox')
    await page.waitForFunction(() => document.querySelector('input[type=checkbox]')?.checked)
    await autostart.click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'set_autostart'))
    assert.deepEqual((await calls()).find(([c]) => c === 'set_autostart'), ['set_autostart', { on: false }])
    await page.getByRole('button', { name: 'Поставить команду ferry' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'install_cli'))
    // Обновление: тихая проверка при запуске уже нашла версию.
    await page.getByText('Есть ferry 0.1.1').waitFor()
    await page.getByRole('button', { name: 'Обновить все устройства' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'update_all'))
    await page.getByRole('button', { name: 'Обновить и перезапустить' }).click()
    await page.getByText('ferry 0.1.0 → 0.1.1: ставлю и перезапускаюсь', { exact: true }).waitFor()
    await fits('настройки')
    await shot('settings')

    await page.getByRole('button',{name:'Прогоны',exact:true}).click()
    await page.locator('.run-row').first().waitFor()
    assert.equal(await page.locator('.run-row').count(),2)
    await page.getByLabel('Поиск прогонов').fill('check-first')
    assert.equal(await page.locator('.run-row').count(),1)
    await page.getByLabel('Поиск прогонов').fill('')
    await page.locator('.run-row').last().click()
    await page.getByRole('article',{name:'Результаты прогона'}).waitFor()
    await page.getByText('junit.xml',{exact:true}).waitFor()
    await page.getByText('Тесты · 1 в предпросмотре',{exact:true}).click()
    await page.getByText('execution',{exact:true}).waitFor()
    await page.getByText('Ещё 2 тестов доступны в ZIP-отчёте.',{exact:true}).waitFor()
    await page.getByText('Результат этапа',{exact:true}).click()
    assert.equal(await page.locator('.run-detail script').count(),0)
    await page.getByLabel('Базовый прогон').selectOption('check-second')
    await page.getByRole('button',{name:'Сравнить',exact:true}).click()
    await page.getByText('Сопоставимые testcase',{exact:true}).waitFor()
    await page.getByRole('button',{name:'Скачать отчёт ZIP',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c,a])=>c==='dashboard' && a.request.action==='export' && a.request.id==='check-first'))
    await fits('центр прогонов'); await shot('runs')
    await page.getByRole('button',{name:'Подключить агента',exact:true}).click()
    await page.getByRole('heading',{name:'Claude Code'}).waitFor()
    await page.locator('.agent-option').first().getByRole('button',{name:'Настроить',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c,a])=>c==='dashboard' && a.request.action==='setup_install' && a.request.value==='claude'))
    await page.getByLabel('Устройство первой проверки').selectOption('win-10')
    await page.getByRole('button',{name:'Запустить проверку',exact:true}).click()
    await page.waitForFunction(()=>window.__calls.some(([c,a])=>c==='dashboard' && a.request.action==='setup_smoke' && a.request.value==='win-10'))
    await fits('онбординг'); await shot('onboarding')
    await page.close()
  }

  {
    const page=await browser.newPage({viewport:{width:390,height:844},colorScheme:'dark'})
    page.on('pageerror',e=>failures.push(`web: ${e.message}`))
    const calls=[]
    await page.route('**/api',async route=>{
      const request=route.request();assert.equal(request.headers().authorization,'Bearer web-test')
      const body=request.postDataJSON();calls.push(body)
      const value=body.action==='snapshot' ? snapshot : body.action==='about' ? {read_only:true,web:true} : fixtures.dashboard[body.action]
      await route.fulfill({contentType:'application/json',body:JSON.stringify(value)})
    })
    await page.goto(url+'#token=web-test')
    await page.locator('.tile').first().waitFor()
    assert.equal(new URL(page.url()).hash,'','token must leave address bar')
    assert.equal(await page.getByRole('button',{name:'Настройки'}).count(),0)
    await page.getByRole('button',{name:'Прогоны',exact:true}).click()
    await page.locator('.run-row').last().click()
    await page.getByText('Панель открыта для просмотра.',{exact:false}).waitFor()
    assert.equal(await page.getByRole('button',{name:'Отменить прогон',exact:true}).count(),0)
    const [scroll,client]=await page.locator('main').evaluate(m=>[m.scrollWidth,m.clientWidth]);assert.ok(scroll<=client,'narrow web overflows')
    if(SHOTS) await page.screenshot({path:join(SHOTS,'web-narrow.png')})
    await page.getByRole('button',{name:'Подключить агента',exact:true}).click()
    await page.getByRole('heading',{name:'Настройка на компьютере с ferry'}).waitFor()
    assert.equal(calls.filter(c=>c.action.startsWith('setup')).length,0,'web must not install')
    await page.close()
  }

  // Мак без доступов: страница разрешений открывается сама, «Выдать» просит нужный доступ.
  {
    const page = await browser.newPage({ viewport: { width: win0.minWidth, height: win0.minHeight } })
    page.on('pageerror', (e) => failures.push(`разрешения: ${e.message}`))
    await page.addInitScript((fx) => {
      window.__calls = []
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} }
      window.__TAURI_INTERNALS__ = {
        transformCallback: () => 0,
        invoke: async (cmd, args = {}) => {
          if (cmd !== 'plugin:event|listen') window.__calls.push([cmd, args])
          return cmd in fx ? fx[cmd] : null
        },
      }
    }, { ...fixtures, permissions: { screen: false, input: true } })
    await page.goto(url)
    await page.getByRole('heading', { name: 'Разрешения' }).waitFor()
    await page.getByRole('button', { name: 'Разрешения · нужно выдать' }).waitFor()
    await page.getByRole('button', { name: 'Выдать', exact: true }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'ask_permission'))
    const calls = await page.evaluate(() => window.__calls)
    assert.deepEqual(calls.find(([c]) => c === 'ask_permission'), ['ask_permission', { kind: 'screen' }])
    await page.getByRole('button', { name: 'Перезапустить ferry' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'restart_app'))
    if (SHOTS) await page.screenshot({ path: join(SHOTS, 'permissions.png') })
    await page.close()
  }

  // «Связать устройство…» из трея, когда окна ещё не было: окно создаётся сразу на связывании.
  {
    const page = await browser.newPage({ viewport: { width: win0.minWidth, height: win0.minHeight } })
    page.on('pageerror', (e) => failures.push(`связывание при создании: ${e.message}`))
    await page.addInitScript((fx) => {
      window.FERRY_PAIRING = true
      window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} }
      window.__TAURI_INTERNALS__ = {
        transformCallback: () => 0,
        invoke: async (cmd) => (cmd in fx ? fx[cmd] : null),
      }
    }, fixtures)
    await page.goto(url)
    await page.getByRole('heading', { name: 'Связать устройство' }).waitFor()
    await page.close()
  }
} finally {
  await browser.close()
  server.close()
}
assert.deepEqual(failures, [], 'ошибки JS на странице')
console.log('ui: ok')
