// Перед сборкой установщика (`bun run release:*`): CLI ferry под ту же платформу — рядом с
// приложением (sidecar `binaries/ferry-<платформа>`). Платформу Tauri передаёт в
// TAURI_ENV_TARGET_TRIPLE. WebView2Loader.dll для Windows на gnu сборщик Tauri кладёт сам.
import { execFileSync } from 'node:child_process'
import { copyFileSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const APP = join(dirname(fileURLToPath(import.meta.url)), '..')
const ROOT = join(APP, '../..')
const host = execFileSync('rustc', ['-vV']).toString().match(/host: (\S+)/)[1]
const triple = process.env.TAURI_ENV_TARGET_TRIPLE || host
const exe = triple.includes('windows') ? '.exe' : ''
const out = join(APP, 'src-tauri/binaries')
mkdirSync(out, { recursive: true })

execFileSync('cargo', ['build', '--release', '-p', 'ferry', '--target', triple], { cwd: ROOT, stdio: 'inherit' })
copyFileSync(join(ROOT, 'target', triple, 'release', `ferry${exe}`), join(out, `ferry-${triple}${exe}`))

console.log(`ferry: CLI для ${triple} → src-tauri/binaries`)
