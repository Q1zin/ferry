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
    const want = [Math.round(((cx - box.x) * 1366) / box.width), Math.round(((cy - box.y) * 768) / box.height)]
    await page.mouse.click(cx, cy)
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'input'))
    const [, click] = (await calls()).find(([c]) => c === 'input')
    assert.equal(click.id, WIN)
    assert.equal(click.args[0], 'click')
    assert.ok(Math.abs(click.args[1] - want[0]) <= 1 && Math.abs(click.args[2] - want[1]) <= 1, `клик ${click.args}, ждали ${want}`)
    await page.getByPlaceholder('Ввести текст на устройстве').fill('Привет')
    await page.getByRole('button', { name: 'Ввести' }).click()
    await page.waitForFunction(() => window.__calls.some(([c, a]) => c === 'input' && a.args[0] === 'type'))
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
