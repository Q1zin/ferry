// После `bun run release:mac` и `release:windows`: всё для релиза v<версия> в одном каталоге —
// установщики, пакеты обновления и latest.json для updater (подписи — из .sig, которые сборка
// кладёт рядом с пакетами). Публикует релиз человек: gh release create v<версия> <каталог>/*.
// FERRY_RELEASE_URL — откуда updater будет качать пакеты; по умолчанию — из этого релиза.
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { basename, dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '../../..')
const version = readFileSync(join(ROOT, 'Cargo.toml'), 'utf8').match(/^version = "(.+)"/m)[1]
const base = process.env.FERRY_RELEASE_URL ?? `https://github.com/Q1zin/ferry/releases/download/v${version}`
const out = join(ROOT, 'target', `release-v${version}`)
const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64'
const mac = join(ROOT, 'target/release/bundle')
const win = join(ROOT, 'target/x86_64-pc-windows-gnu/release/bundle')

// Пакет обновления для мака — без версии в имени: сверяем её по Info.plist, чтобы не взять старый.
const plist = join(mac, 'macos/ferry.app/Contents/Info.plist')
const macFresh = existsSync(plist) && readFileSync(plist, 'utf8').includes(`<string>${version}</string>`)

// Платформа updater, пакет обновления, имя в релизе, установщик для новых устройств.
const builds = [
  [`darwin-${arch}`, macFresh && join(mac, 'macos/ferry.app.tar.gz'), `ferry_${version}_${arch}.app.tar.gz`, join(mac, `dmg/ferry_${version}_${arch}.dmg`)],
  ['windows-x86_64', join(win, `nsis/ferry_${version}_x64-setup.exe`), `ferry_${version}_x64-setup.exe`, null],
]

mkdirSync(out, { recursive: true })
const platforms = {}
for (const [platform, pkg, name, installer] of builds) {
  if (!pkg || !existsSync(`${pkg}.sig`)) {
    console.log(`ferry: ${platform} — нет подписанной сборки ${version}, пропускаю`)
    continue
  }
  copyFileSync(pkg, join(out, name))
  if (installer && existsSync(installer)) copyFileSync(installer, join(out, basename(installer)))
  platforms[platform] = { signature: readFileSync(`${pkg}.sig`, 'utf8'), url: `${base}/${name}` }
}
if (!Object.keys(platforms).length) {
  console.error('ferry: нет ни одной сборки — сначала bun run release:mac или release:windows')
  process.exit(1)
}
const manifest = { version, pub_date: new Date().toISOString(), platforms }
writeFileSync(join(out, 'latest.json'), `${JSON.stringify(manifest, null, 2)}\n`)
console.log(`ferry: релиз v${version} (${Object.keys(platforms).join(', ')}) → ${out}`)
