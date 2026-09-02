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
}
const fixtures = {
  state: snapshot,
  open_pairing: { code: '4821-0937', ticket: `ferry:${snapshot.id}:4821-0937`, seconds_left: 600 },
  pair: { id: NEW, name: 'office-pc', online: true, status: { ...win, name: 'office-pc' }, direct: true, rtt_ms: 3 },
  autostart: true,
  version: '0.1.0',
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
    await fits('чужое устройство')
    await shot('peer')

    // Устройство не на связи: управлять нечем, но видно почему.
    await page.locator('nav .node-link', { hasText: 'win-11' }).click()
    await page.getByText('Не на связи: выключено').waitFor()
    assert.ok(await page.getByRole('button', { name: 'узел', exact: true }).isDisabled())

    // Это устройство: режим выбирают здесь.
    await page.locator('nav .node-link', { hasText: 'macbook' }).click()
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
    const box = page.getByRole('checkbox')
    await page.waitForFunction(() => document.querySelector('input[type=checkbox]')?.checked)
    await box.click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'set_autostart'))
    assert.deepEqual((await calls()).find(([c]) => c === 'set_autostart'), ['set_autostart', { on: false }])
    await fits('настройки')
    await shot('settings')
    await page.close()
  }
} finally {
  await browser.close()
  server.close()
}
assert.deepEqual(failures, [], 'ошибки JS на странице')
console.log('ui: ok')
