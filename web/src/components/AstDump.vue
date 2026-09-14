<script setup lang="ts">
import type { ParseError } from '../api/types'

defineProps<{ statements: { debug: string }[]; error: ParseError | null }>()

const stageLabel: Record<ParseError['stage'], string> = {
  lex: '词法阶段',
  parse: '语法阶段',
}
</script>

<template>
  <div class="ast">
    <el-alert
      v-if="error"
      type="warning"
      show-icon
      :closable="false"
      :title="`${stageLabel[error.stage]}错误`"
      :description="error.message"
    />
    <el-empty
      v-else-if="!statements.length"
      description="没有语句"
      :image-size="48"
    />
    <template v-else>
      <!-- `{:#?}` for now: the whole point is to show the tree the parser
           actually built, and the engine does not serialise it structurally
           yet. -->
      <pre v-for="(statement, index) in statements" :key="index" class="dump">{{ statement.debug }}</pre>
    </template>
  </div>
</template>

<style scoped>
.ast {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.dump {
  margin: 0;
  padding: 10px 12px;
  background: var(--el-fill-color-light);
  border-radius: 4px;
  font-family: var(--el-font-family-mono, ui-monospace, monospace);
  font-size: 12px;
  line-height: 1.5;
  overflow-x: auto;
  white-space: pre;
}
</style>
