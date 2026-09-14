<script setup lang="ts">
import type { Diagnostic } from '@codemirror/lint'
import { onMounted, ref, watch } from 'vue'

import { ApiError, parseSql, query } from '../api/client'
import type { ParseTrace, ResultSet } from '../api/types'
import AstDump from '../components/AstDump.vue'
import ResultGrid from '../components/ResultGrid.vue'
import SchemaTree from '../components/SchemaTree.vue'
import SqlEditor from '../components/SqlEditor.vue'
import TokenStream from '../components/TokenStream.vue'
import { loadDraft, saveDraft } from '../draft'
import { markerFor } from '../sqlpos'
import { useSession } from '../stores/session'

/// Shown the first time this tab opens the console; after that the draft wins.
const SAMPLE = 'select 1 as one;'
const DRAFT_KEY = 'chibidb.draft.sql'

const session = useSession()
const tree = ref<InstanceType<typeof SchemaTree> | null>(null)

const sql = ref(loadDraft(DRAFT_KEY, SAMPLE))
const results = ref<ResultSet[]>([])
const error = ref('')
const running = ref(false)
const elapsedMs = ref<number | null>(null)

/// The last compiler trace, kept for the detail panel.
const trace = ref<ParseTrace | null>(null)
const showDetail = ref(false)
/// Why the editor is not being underlined -- the parse API being switched off,
/// or a transport failure. Without this the reader is left wondering why
/// nothing ever turns red.
const parseNotice = ref('')

watch(sql, (text) => saveDraft(DRAFT_KEY, text))

/// Runs the compiler and reports what to underline.
///
/// Called by the editor on a debounce, and never throws: a failed diagnosis must
/// not break typing, and the editor has nowhere to put an exception.
async function diagnose(text: string): Promise<Diagnostic[]> {
  if (!text.trim()) {
    trace.value = null
    parseNotice.value = ''
    return []
  }
  try {
    const parsed = await parseSql(text)
    // A slower earlier request can land after a newer one; taking its answer
    // would put a stale trace (and a stale marker) on screen.
    if (text !== sql.value) return []
    trace.value = parsed
    parseNotice.value = ''
    if (!parsed.error) return []
    const marker = markerFor(text, parsed.error)
    // A null position means the engine had nowhere to point -- a runtime
    // complaint, or a compile problem with no offending token. The message is
    // still worth showing, the alert below does that; there is just nothing to
    // underline.
    return marker
      ? [{ from: marker.from, to: marker.to, severity: 'error', message: marker.message }]
      : []
  } catch (e) {
    trace.value = null
    parseNotice.value =
      e instanceof ApiError && e.status === 404
        ? e.message
        : `解析失败：${e instanceof Error ? e.message : String(e)}`
    return []
  }
}

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
  }
}

function onSelected(name: string): void {
  session.setCurrentDb(name)
}

onMounted(session.bootstrap)
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
        <span class="spacer" />
        <el-button link size="small" @click="showDetail = !showDetail">
          {{ showDetail ? '▾' : '▸' }} 解析详情
        </el-button>
      </div>

      <SqlEditor v-model="sql" :diagnose="diagnose" @run="run" />

      <el-alert
        v-if="parseNotice"
        type="info"
        show-icon
        :closable="false"
        title="没有语法检查"
        :description="parseNotice"
      />
      <el-alert
        v-else-if="trace?.error"
        type="warning"
        show-icon
        :closable="false"
        :title="`${trace.error.stage === 'lex' ? '词法' : '语法'}错误`"
        :description="trace.error.message"
      />

      <el-collapse-transition>
        <div v-show="showDetail" class="detail">
          <section>
            <h3>Token 流</h3>
            <TokenStream :tokens="trace?.tokens ?? []" />
          </section>
          <section>
            <h3>AST</h3>
            <AstDump :statements="trace?.statements ?? []" :error="trace?.error ?? null" />
          </section>
        </div>
      </el-collapse-transition>

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

.spacer {
  flex: 1;
}

.hint {
  color: var(--el-text-color-secondary);
  font-size: 12px;
}

.detail {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 20px;
  align-items: start;
}

.detail h3 {
  margin: 0 0 8px;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

@media (max-width: 1100px) {
  .detail {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
