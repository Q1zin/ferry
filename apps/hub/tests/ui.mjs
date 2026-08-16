// Смоук-тест пульта в Chromium (`bun run test:ui` после `bun run build`).
// Мост Tauri подменён: invoke отвечает фикстурами в форме ответов бэкенда и записывает все
// вызовы — проверяем отрисовку и то, какие команды и с какими аргументами уходят на узел.
// Скриншоты экранов — в FERRY_UI_SHOTS, если задан.
import assert from 'node:assert/strict'
import { readFileSync, existsSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'
import { chromium } from 'playwright-core'

const DIST = join(dirname(fileURLToPath(import.meta.url)), '../dist')
const SHOTS = process.env.FERRY_UI_SHOTS

// Экран узла 1366×768 с меткой: по ней видно, что снимок показан.
const screen = new Resvg(
  `<svg xmlns="http://www.w3.org/2000/svg" width="1366" height="768">
    <rect width="1366" height="768" fill="#1d4f91"/><rect x="221" y="125" width="474" height="531" fill="#fff"/>
    <text x="240" y="160" font-size="22" font-family="Helvetica">Блокнот на узле</text></svg>`,
  { font: { loadSystemFonts: true } },
).render().asPng()

const fixtures = {
  nodes: [
    {
      name: 'win-10', addr: '192.168.100.21', user: 'q1zin', paused: false, online: true,
      summary: 'MINGW64_NT-10.0-17763, рабочий стол отвечает', mode: 'работает', desk: true,
      latency_ms: 14, warnings: ['на диске 6 ГБ — для сборки с CEF мало'],
    },
    {
      name: 'win-11', addr: '192.168.100.30', user: 'dev', paused: false, online: true,
      summary: 'MINGW64_NT-10.0-22631, на узле: не трогать рабочий стол', mode: 'не трогать рабочий стол',
      desk: false, latency_ms: 9, warnings: ['на узле: не трогать рабочий стол'],
    },
    {
      name: 'mac-mini', addr: '100.71.97.70', user: 'me', paused: true, online: false,
      summary: 'пауза на хабе', mode: null, desk: false, latency_ms: null, warnings: [],
    },
  ],
  detail: {
    properties: [
      ['система', 'Майкрософт Windows 10 Корпоративная LTSC (17763)'],
      ['процессор', 'Intel(R) Pentium(R) CPU B960 @ 2.20GHz, потоков: 2'],
      ['память', 'свободно 0,8 из 3,9 ГБ'],
      ['диск', 'свободно 6,5 из 118,7 ГБ'],
      ['питание', 'от сети'],
      ['ferry', '0.1.0'],
    ],
    warnings: ['на диске 6 ГБ — для сборки с CEF мало'],
  },
  jobs: [
    { name: 'app', label: 'работает', ago: '2 мин назад', running: true, cmd: './logarithmica.exe --remote-debugging-port=9322' },
    { name: 'build', label: 'завершилась, код 0', ago: '9 мин назад', running: false, cmd: 'cargo build' },
  ],
  job_log: 'Compiling ferry v0.1.0\nFinished in 41s\n',
  screenshot: `data:image/png;base64,${Buffer.from(screen).toString('base64')}`,
  trusted: ['git.logarithmica.ru'],
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

const browser = await chromium.launch()
const failures = []
try {
  for (const scheme of ['light', 'dark']) {
    const page = await browser.newPage({ viewport: { width: 1100, height: 720 }, colorScheme: scheme })
    page.on('pageerror', (e) => failures.push(`${scheme}: ${e.message}`))
    await page.addInitScript((fx) => {
      window.__calls = []
      window.__TAURI_INTERNALS__ = {
        invoke: async (cmd, args) => {
          window.__calls.push([cmd, args ?? {}])
          return cmd in fx ? fx[cmd] : ''
        },
        transformCallback: () => 0,
      }
    }, fixtures)
    const calls = () => page.evaluate(() => window.__calls.splice(0))
    const shot = (name) => SHOTS && page.screenshot({ path: join(SHOTS, `${scheme}-${name}.png`) })

    // Главный экран: карточки узлов со статусом и предупреждениями.
    await page.goto(url)
    await page.getByText('на диске 6 ГБ — для сборки с CEF мало').first().waitFor()
    assert.equal(await page.locator('.tile').count(), 3)
    await shot('home')

    // Страница узла, «Обзор»: свойства и действия.
    await page.locator('nav .node-link', { hasText: 'win-10' }).click()
    await page.getByText('Pentium').waitFor()
    await shot('overview')
    await calls()
    await page.getByRole('button', { name: 'Пауза на хабе' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'pause'))
    assert.deepEqual((await calls()).find(([c]) => c === 'pause'), ['pause', { host: 'win-10', paused: true }])

    // «Экран»: клик по снимку уходит на узел в пикселях его экрана.
    await page.getByRole('button', { name: 'Экран', exact: true }).click()
    const img = page.locator('.frame img')
    await img.waitFor()
    await page.waitForFunction(() => document.querySelector('.frame img')?.naturalWidth > 0)
    const box = await img.boundingBox()
    await calls()
    // Браузер отдаёт координаты мыши целыми — ожидание считаем от того же целого клика.
    const [cx, cy] = [Math.round(box.x + box.width * 0.5), Math.round(box.y + box.height * 0.25)]
    const want = [Math.round(((cx - box.x) * 1366) / box.width), Math.round(((cy - box.y) * 768) / box.height)]
    await page.mouse.click(cx, cy)
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'input'))
    const [, click] = (await calls()).find(([c]) => c === 'input')
    const [verb, x, y] = click.args
    assert.equal(verb, 'click')
    assert.ok(
      Math.abs(Number(x) - want[0]) <= 1 && Math.abs(Number(y) - want[1]) <= 1,
      `клик в ${x},${y}, ждали ${want}`,
    )
    await page.getByPlaceholder('Ввести текст на узле').fill('Привет, узел')
    await page.getByRole('button', { name: 'Ввести' }).click()
    await page.getByRole('button', { name: 'Ctrl+A' }).click()
    await page.waitForFunction(() => window.__calls.filter(([c]) => c === 'input').length >= 2)
    const inputs = (await calls()).filter(([c]) => c === 'input').map(([, a]) => a.args)
    assert.deepEqual(inputs.slice(0, 2), [['type', 'Привет, узел'], ['key', 'ctrl+a']])
    await shot('screen')

    // «Задачи»: список, лог, запуск.
    await page.getByRole('button', { name: 'Задачи', exact: true }).click()
    await page.getByText('logarithmica.exe').waitFor()
    await page.getByRole('row', { name: /build/ }).getByRole('button', { name: 'Лог' }).click()
    await page.getByText('Finished in 41s').waitFor()
    await page.getByPlaceholder('имя: app').fill('tests')
    await page.getByPlaceholder('команда').fill('bun test')
    await calls()
    await page.getByRole('button', { name: 'Запустить' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'start_job'))
    assert.deepEqual((await calls()).find(([c]) => c === 'start_job'), [
      'start_job', { host: 'win-10', name: 'tests', cmd: 'bun test' },
    ])
    await shot('jobs')

    // Узел в режиме «не трогать рабочий стол»: экран не трогаем совсем.
    await page.locator('nav .node-link', { hasText: 'win-11' }).click()
    await page.getByRole('button', { name: 'Экран', exact: true }).click()
    await page.getByText('рабочий стол трогать нельзя').waitFor()
    assert.ok(!(await calls()).some(([c]) => c === 'screenshot'), 'снимок узла в режиме «не трогать»')

    // Настройки: доверенные git-хосты.
    await page.getByRole('button', { name: 'Настройки' }).click()
    await page.getByText('git.logarithmica.ru').waitFor()
    await page.getByPlaceholder('git.example.com').fill('github.com')
    await page.getByRole('button', { name: 'Доверить' }).click()
    await page.waitForFunction(() => window.__calls.some(([c]) => c === 'trust'))
    await shot('settings')
    await page.close()
  }
} finally {
  await browser.close()
  server.close()
}
assert.deepEqual(failures, [], 'ошибки JS на странице')
console.log('ui: ok')
