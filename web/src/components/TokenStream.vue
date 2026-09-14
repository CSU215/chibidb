<script setup lang="ts">
import type { Token } from '../api/types'

defineProps<{ tokens: Token[] }>()
</script>

<template>
  <div class="tokens">
    <div class="meta">{{ tokens.length }} 个 token</div>
    <el-table :data="tokens" size="small" border max-height="50vh">
      <el-table-column prop="pos" label="偏移" width="70" />
      <el-table-column prop="kind" label="类别" width="90">
        <template #default="{ row }">
          <el-tag size="small" :type="row.kind === 'Punct' ? 'info' : 'primary'">
            {{ row.kind }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column prop="text" label="词素" min-width="160">
        <template #default="{ row }">
          <code>{{ row.text }}</code>
        </template>
      </el-table-column>
    </el-table>
  </div>
</template>

<style scoped>
.tokens {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.meta {
  color: var(--el-text-color-secondary);
  font-size: 12px;
}
</style>
