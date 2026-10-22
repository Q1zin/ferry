<script setup lang="ts">
import { nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { api, onHumanQuestions, type HumanQuestion } from '../api'

const emit = defineEmits<{ say: [text: string, error?: boolean] }>()
const questions = ref<HumanQuestion[]>([])
const busy = ref<string | null>(null)
const dialog = ref<HTMLDialogElement | null>(null)
watch(() => questions.value.length, async (count) => {
  await nextTick()
  if (count && dialog.value && !dialog.value.open) dialog.value.showModal()
  else if (!count) dialog.value?.close()
})
let unlisten: (() => void) | undefined
onMounted(async () => {
  let changed = false
  unlisten = await onHumanQuestions((q) => { changed = true; questions.value = q })
  const initial = (await api.questions().catch(() => [])) ?? []
  if (!changed) questions.value = initial
})
onUnmounted(() => unlisten?.())
async function answer(id: string, reply: 'done' | 'declined') {
  busy.value = id
  try {
    await api.answerQuestion(id, reply)
    questions.value = questions.value.filter((q) => q.id !== id)
  } catch (e) {
    emit('say', String(e), true)
  } finally {
    busy.value = null
  }
}
</script>

<template>
  <dialog ref="dialog" class="human-dialog" aria-labelledby="human-heading" @cancel.prevent>
      <h2 id="human-heading">Агенту нужна ваша помощь</h2>
      <p class="muted">Выполните действие на этом компьютере и сообщите результат. Пароли и коды вводите только в приложении, которое их запросило.</p>
      <article v-for="question in questions" :key="question.id" class="human-question">
        <p class="muted">{{ question.from }}</p>
        <p class="question-text">{{ question.text }}</p>
        <p class="muted">Ответ до {{ new Date(question.expires_at * 1000).toLocaleTimeString() }}</p>
        <div class="human-actions">
          <button :disabled="busy !== null" @click="answer(question.id, 'declined')">Не могу выполнить</button>
          <button class="primary" :disabled="busy !== null" @click="answer(question.id, 'done')">Готово</button>
        </div>
      </article>
  </dialog>
</template>

<style scoped>
.human-dialog::backdrop { background: #0007; }
.human-dialog { color: inherit; width: min(560px, calc(100% - 48px)); max-height: calc(100vh - 48px); overflow: auto; padding: 24px; border-radius: 16px; background: var(--paper); border: 1px solid var(--line); box-shadow: 0 16px 56px #0003; }
.human-dialog h2 { margin-top: 0; }
.human-question { border-top: 1px solid var(--line); padding-top: 8px; margin-top: 16px; }
.question-text { white-space: pre-wrap; overflow-wrap: anywhere; font-size: 17px; }
.human-actions { display: flex; justify-content: flex-end; gap: 10px; flex-wrap: wrap; }
</style>
