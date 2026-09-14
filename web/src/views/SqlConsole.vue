<script setup lang="ts">
import { nextTick, onMounted, ref, watch } from 'vue'

import { ApiError, query } from '../api/client'
import type { ResultSet } from '../api/types'
import ResultGrid from '../components/ResultGrid.vue'
import SchemaTree from '../components/SchemaTree.vue'
import { loadDraft, saveDraft } from '../draft'
import { useSession } from '../stores/session'

const session = useSession()
const tree = ref<InstanceType<typeof SchemaTree> | null>(null)

/// Shown the first time this tab opens the console; after that the draft wins.
const SAMPLE = 'select 1 as one;'
const DRAFT_KEY = 'chibidb.draft.sql'

const sql = ref(loadDraft(DRAFT_KEY, SAMPLE))
// Saved on every change rather than on unload: a reload is not the only way to
// lose the text, and the string is tiny.
watch(sql, (text) => saveDraft(DRAFT_KEY, text))
const results = ref<ResultSet[]>([])
const error = ref('')
const running = ref(false)
const elapsedMs = ref<number | null>(null)
const editor = ref<HTMLTextAreaElement | null>(null)

onMounted(session.bootstrap)

async function run(): Promise<void> {
  const text = sql.value.trim()
  if (!text || running.value) return

  running.value = true
  error.value = ''
  const started = performance.now()
  try {
    results.value = await query(text)
    // The engine may have replaced our session id (restart, or expiry); keep the
    // badge honest.
    session.refreshId()
    // `use` changes the session's database, so the header follows it.
    const changed = /\buse\s+([A-Za-z0-9_]+)/i.exec(text)
    if (changed) {
      session.setCurrentDb(changed[1])
      void tree.value?.loadDatabases()
    }
    // DDL changes what the tree should show.
    if (/\b(create|drop|alter)\b/i.test(text)) void tree.value?.loadDatabases()
  } catch (e) {
    results.value = []
    error.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
  } finally {
    elapsedMs.value = performance.now() - started
    running.value = false
    // Focus back so the next statement can be typed straight away.
    await nextTick()
    editor.value?.focus()
  }
}

/// Ctrl/Cmd+Enter runs, and so does Escape-free plain Enter with the modifier.
function onKeydown(event: KeyboardEvent): void {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
    event.preventDefault()
    void run()
  }
}

function onSelected(name: string): void {
  session.setCurrentDb(name)
}
</script>

<template>
  <div class="console">
    <aside class="side">
      <SchemaTree ref="tree" @selected="onSelected" />
    </aside>

    <section class="main">
      <div class="bar">
        <el-button type="primary" :loading="running" @click="run">执行</el-button>
        <span class="hint">Ctrl / ⌘ + Enter</span>
        <span v-if="elapsedMs !== null" class="hint">{{ elapsedMs.toFixed(1) }} ms</span>
      </div>

      <textarea
        ref="editor"
        v-model="sql"
        class="editor"
        spellcheck="false"
        placeholder="在这里输入 SQL，Ctrl/⌘ + Enter 执行"
        @keydown="onKeydown"
      />

      <el-alert v-if="error" type="error" show-icon :closable="false" :title="error" />

      <ResultGrid v-if="results.length" :results="results" />
      <el-empty v-else-if="!error" description="还没有结果" :image-size="72" />
    </section>
  </div>
</template>

<style scoped>
.console {
  display: grid;
  grid-template-columns: 280px 1fr;
  gap: 20px;
  align-items: start;
}

.side {
  padding: 12px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  background: var(--el-bg-color);
}

.main {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
}

.bar {
  display: flex;
  align-items: center;
  gap: 12px;
}

.hint {
  color: var(--el-text-color-secondary);
  font-size: 12px;
}

.editor {
  width: 100%;
  min-height: 180px;
  padding: 12px;
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  font-family: var(--el-font-family-mono, ui-monospace, monospace);
  font-size: 13px;
  line-height: 1.6;
  resize: vertical;
  outline: none;
}

.editor:focus {
  border-color: var(--el-color-primary);
}
</style>
