// Иконки ferry из исходников: `bun run icons` в apps/ferry.
// - иконка приложения хаба → src-tauri/icons (все размеры — через `tauri icon`);
// - иконки трея узла (по режиму, 16–32 px под масштаб экрана) → crates/ferry-core/assets/tray,
//   ferry.exe вшивает их в себя.
import { Resvg } from '@resvg/resvg-js'
import { execFileSync } from 'node:child_process'
import { mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const HUB = join(dirname(fileURLToPath(import.meta.url)), '..')
const TRAY = join(HUB, '../../crates/ferry-core/assets/tray')

const INK = '#1F2937'
const CORAL = '#F25C3A'
const PAPER = '#F7F2EA'
const svg = body => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">${body}</svg>`
const line = 'stroke-linecap="round" stroke-linejoin="round" fill="none"'

// Маршрут переправы: от точки (хаб) к кольцу (узел). Поля — по сетке macOS (824 из 1024).
const appIcon = svg(`<defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="${PAPER}"/><stop offset="1" stop-color="#EBE1D1"/></linearGradient>
    <radialGradient id="glow" cx="0.28" cy="0.18" r="0.85">
      <stop offset="0" stop-color="#fff" stop-opacity="0.55"/><stop offset="1" stop-color="#fff" stop-opacity="0"/></radialGradient>
  </defs>
  <rect x="100" y="100" width="824" height="824" rx="185" fill="url(#bg)"/>
  <rect x="100" y="100" width="824" height="824" rx="185" fill="url(#glow)"/>
  <g transform="translate(512 512) scale(0.9) translate(-512 -512)">
    <path d="M306 704 C430 704 404 536 512 512 C620 488 594 326 718 326" stroke="${INK}" stroke-width="54" ${line}/>
    <circle cx="306" cy="704" r="64" fill="${INK}"/>
    <circle cx="718" cy="326" r="68" stroke="${CORAL}" stroke-width="50" fill="${PAPER}"/>
  </g>`)

// Режим узла — цветом и формой: «не трогать» фиолетовый, как «Не беспокоить» в macOS;
// пауза — пустое кольцо, чтобы читалось и без цвета.
const modes = {
  active: { fill: CORAL, ink: '#FFFFFF' },
  'no-desktop': { fill: '#7C5CFC', ink: '#FFFFFF' },
  paused: { ring: '#9AA3B2', ink: '#9AA3B2' },
}
const route = ink => `
  <path d="M262 742 C420 742 392 520 512 512 C632 504 604 282 762 282" stroke="${ink}" stroke-width="104" ${line}/>
  <circle cx="262" cy="742" r="112" fill="${ink}"/>
  <circle cx="762" cy="282" r="112" fill="${ink}"/>`
const trayIcon = mode => {
  const m = modes[mode]
  const disc = m.ring
    ? `<circle cx="512" cy="512" r="440" fill="none" stroke="${m.ring}" stroke-width="112"/>`
    : `<circle cx="512" cy="512" r="508" fill="${m.fill}"/>`
  const scale = m.ring ? 0.6 : 0.72
  return svg(`${disc}<g transform="translate(512 512) scale(${scale}) translate(-512 -512)">${route(m.ink)}</g>`)
}

const png = (source, width) =>
  new Resvg(source, { fitTo: { mode: 'width', value: width } }).render().asPng()

const appPng = join(HUB, 'src-tauri/icons/app-icon.png')
writeFileSync(appPng, png(appIcon, 1024))
execFileSync('bunx', ['tauri', 'icon', appPng, '-o', join(HUB, 'src-tauri/icons')], { stdio: 'inherit' })
// Мобильных сборок нет; исходник 1024 px — промежуточный.
for (const extra of ['android', 'ios', 'app-icon.png']) {
  rmSync(join(HUB, 'src-tauri/icons', extra), { recursive: true, force: true })
}

mkdirSync(TRAY, { recursive: true })
for (const mode of Object.keys(modes)) {
  for (const size of [16, 20, 24, 32]) {
    writeFileSync(join(TRAY, `${mode}-${size}.png`), png(trayIcon(mode), size))
  }
}
console.log(`трей: ${TRAY}`)
