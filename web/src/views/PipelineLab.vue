<script setup lang="ts">
import { onUnmounted, ref, watch } from 'vue'

import { ApiError, parseSql } from '../api/client'
import type { ParseTrace } from '../api/types'
import AstDump from '../components/AstDump.vue'
import TokenStream from '../components/TokenStream.vue'

/// How long to wait after the last keystroke before asking the engine. The
/// point is to answer while the query is still being written, without sending a
/// request per character.
const DEBOUNCE_MS = 300

const sql = ref('select id, name from t where id > 1 order by id;')
const trace = ref<ParseTrace | null>(null)
const error = ref('')
const parsing = ref(false)

let timer: number | undefined

async function run(): Promise<void> {
  const text = sql.value.trim()
  if (!text) {
    trace.value = null
    error.value = ''
    return
  }
  parsing.value = true
  try {
    // A SQL error is not an exception here: it arrives as `trace.error`, because
    // the engine treats "this does not compile" as an answer rather than a
    // failed request.
    trace.value = await parseSql(text)
    error.value = ''
  } catch (e) {
    trace.value = null
    error.value = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e)
  } finally {
    parsing.value = false
  }
}

watch(sql, () => {
  window.clearTimeout(timer)
  timer = window.setTimeout(run, DEBOUNCE_MS)
})

onUnmounted(() => window.clearTimeout(timer))

void run()
</script>

<template>
  <div class="lab">
    <div class="bar">
      <el-button :loading="parsing" @click="run">解析</el-button>
      <span class="hint">输入停顿 {{ DEBOUNCE_MS }} ms 自动解析（不执行）</span>
    </div>

    <textarea
      v-model="sql"
      class="editor"
      spellcheck="false"
      placeholder="输入 SQL，这里只编译不执行"
    />

    <el-alert v-if="error" type="error" show-icon :closable="false" :title="error" />

    <div v-if="trace" class="panes">
      <section>
        <h3>Token 流</h3>
        <TokenStream :tokens="trace.tokens" />
      </section>
      <section>
        <h3>AST</h3>
        <AstDump :statements="trace.statements" :error="trace.error" />
      </section>
    </div>
  </div>
</template>

<style scoped>
.lab {
  display: flex;
  flex-direction: column;
  gap: 12px;
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
  min-height: 120px;
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

.panes {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 20px;
  align-items: start;
}

.panes h3 {
  margin: 0 0 8px;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

@media (max-width: 1100px) {
  .panes {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
