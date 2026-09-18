<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, type Snapshot } from '../api'

const props = defineProps<{ snap: Snapshot; update: string | null }>()
const emit = defineEmits<{ say: [text: string, error?: boolean]; checked: [version: string | null] }>()

// null — ещё не прочитано: не показывать неверное состояние переключателя.
const autostart = ref<boolean | null>(null)
const cli = ref<{ path: string | null; installed: string | null } | null>(null)
const installing = ref(false)
const updating = ref(false)

onMounted(async () => {
  try {
    ;[autostart.value, cli.value] = await Promise.all([api.autostart(), api.cli()])
  } catch (e) {
    emit('say', String(e), true)
  }
})

async function installCli() {
  installing.value = true
  try {
    emit('say', await api.installCli())
    cli.value = await api.cli()
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    installing.value = false
  }
}

async function checkUpdate() {
  updating.value = true
  try {
    const version = await api.checkUpdate()
    emit('checked', version)
    if (!version) emit('say', `ferry ${props.snap.me.version} — последняя версия`)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    updating.value = false
  }
}

// После ответа приложение перезапускается само: кнопку не возвращаем.
async function installUpdate() {
  updating.value = true
  try {
    emit('say', await api.installUpdate())
  } catch (e) {
    emit('say', String(e), true)
    updating.value = false
  }
}

async function toggle() {
  const on = !autostart.value
  try {
    await api.setAutostart(on)
    autostart.value = on
  } catch (e) {
    emit('say', String(e), true)
  }
}
</script>

<template>
  <section class="settings">
    <h1>Настройки</h1>

    <div class="card">
      <label class="row">
        <input type="checkbox" :checked="!!autostart" :disabled="autostart === null" @change="toggle" />
        Запускать ferry при входе в систему
      </label>
      <p class="muted small">
        Без этого устройство не на связи, пока ferry не откроют вручную. Окно при этом не появляется — только
        иконка в трее.
      </p>
    </div>

    <div class="card">
      <h3>Команда ferry в терминале</h3>
      <p class="muted small">
        Через неё работают Claude и скрипты: <code>ferry run all -- bun test</code>, снимки, ввод, туннели.
      </p>
      <template v-if="cli?.path">
        <p v-if="cli.installed" class="small">Стоит: <code>{{ cli.installed }}</code></p>
        <div class="row">
          <button :disabled="installing" :aria-busy="installing" @click="installCli">
            {{ cli.installed ? 'Переустановить' : 'Поставить команду ferry' }}
          </button>
        </div>
      </template>
      <p v-else-if="cli" class="muted small">
        В этой сборке CLI рядом с приложением нет — он едет с установщиком. Из исходников:
        <code>cargo install --path crates/ferry</code>.
      </p>
    </div>

    <div class="card">
      <h3>Обновления</h3>
      <p v-if="update">Есть ferry {{ update }}, здесь — {{ snap.me.version }}.</p>
      <p class="muted small">
        Новые версии — из релизов ferry на GitHub. Связанные устройства обновляются отсюда же:
        <code>ferry update all</code>.
      </p>
      <div class="row">
        <button v-if="update" :disabled="updating" :aria-busy="updating" @click="installUpdate">
          Обновить и перезапустить
        </button>
        <button v-else :disabled="updating" :aria-busy="updating" @click="checkUpdate">Проверить</button>
      </div>
    </div>

    <div class="card">
      <h3>О ferry</h3>
      <p>Версия {{ snap.me.version }}. На связанных устройствах она должна совпадать.</p>
      <p class="muted small">
        Ключ этого устройства: <code>{{ snap.id }}</code>
      </p>
      <p class="muted small">Всё, что делает приложение, умеет и CLI: <code>ferry</code> без аргументов — список команд.</p>
    </div>
  </section>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 680px;
}

.card {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.card h3,
.card p {
  margin: 0;
}

.small {
  font-size: 12px;
}

code {
  overflow-wrap: anywhere;
}
</style>
