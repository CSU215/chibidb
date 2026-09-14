<script setup lang="ts">
import type { ResultSet } from '../api/types'
import { display } from '../format'

defineProps<{ results: ResultSet[] }>()
</script>

<template>
  <div class="results">
    <template v-for="(result, index) in results" :key="index">
      <el-alert
        v-if="result.type === 'message'"
        :title="result.message"
        type="success"
        show-icon
        :closable="false"
      />
      <div v-else class="grid">
        <div class="grid-meta">
          {{ result.rows.length }} 行 × {{ result.columns.length }} 列
        </div>
        <el-table :data="result.rows" border stripe size="small" max-height="60vh">
          <el-table-column
            v-for="(column, c) in result.columns"
            :key="c"
            :label="column"
            min-width="120"
          >
            <template #default="{ row }">
              <span :class="{ null: row[c] === null }">{{ display(row[c]) }}</span>
            </template>
          </el-table-column>
        </el-table>
      </div>
    </template>
  </div>
</template>

<style scoped>
.results {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.grid-meta {
  color: var(--el-text-color-secondary);
  font-size: 12px;
  margin-bottom: 4px;
}

.null {
  color: var(--el-text-color-placeholder);
  font-style: italic;
}
</style>
