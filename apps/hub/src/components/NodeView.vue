<script setup lang="ts">
import { ref } from 'vue'
import { tone, type NodeRow } from '../api'
import Jobs from './Jobs.vue'
import Overview from './Overview.vue'
import Screen from './Screen.vue'

defineProps<{ node: NodeRow }>()
const emit = defineEmits<{
  say: [text: string, error?: boolean]
  changed: []
  renamed: [name: string]
  gone: []
}>()

const tabs = [
  { id: 'overview', title: 'Обзор' },
  { id: 'screen', title: 'Экран' },
  { id: 'jobs', title: 'Задачи' },
] as const
const tab = ref<(typeof tabs)[number]['id']>('overview')
</script>

<template>
  <section class="node-page">
    <header>
      <div class="row">
        <span class="dot" :class="tone(node)"></span>
        <h1>{{ node.name }}</h1>
        <span class="muted">{{ node.user }}@{{ node.addr }}</span>
      </div>
      <p class="muted summary">
        <template v-if="node.paused">пауза на хабе</template>
        <template v-else-if="node.status">
          {{ node.status.summary }}
          <template v-if="node.status.latency_ms != null"> · {{ node.status.latency_ms }} мс</template>
        </template>
        <span v-else class="skeleton"></span>
      </p>
    </header>

    <div class="tabs">
      <button
        v-for="t in tabs"
        :key="t.id"
        :class="{ active: tab === t.id }"
        @click="tab = t.id"
      >
        {{ t.title }}
      </button>
    </div>

    <Overview
      v-if="tab === 'overview'"
      :node="node"
      @say="(t, e) => emit('say', t, e)"
      @changed="emit('changed')"
      @renamed="(n) => emit('renamed', n)"
      @gone="emit('gone')"
    />
    <Screen v-else-if="tab === 'screen'" :node="node" @say="(t, e) => emit('say', t, e)" />
    <Jobs v-else :node="node" @say="(t, e) => emit('say', t, e)" />
  </section>
</template>

<style scoped>
/* Во всю высоту main: «Экран» тянет снимок под окно. */
.node-page {
  flex: 1 0 auto;
  display: flex;
  flex-direction: column;
}

header {
  margin-bottom: 16px;
}

header h1 {
  font-size: 22px;
}

.summary {
  margin: 4px 0 0 18px;
  min-height: 1lh;
}

.tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--line);
  margin-bottom: 18px;
}

.tabs button {
  border: none;
  border-bottom: 2px solid transparent;
  border-radius: 0;
  background: none;
  padding: 8px 14px;
  color: var(--muted);
}

.tabs button.active {
  color: var(--ink);
  border-bottom-color: var(--coral);
}
</style>
