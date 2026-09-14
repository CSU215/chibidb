<script setup lang="ts">
import { computed } from 'vue'

import type { PlanReport } from '../api/types'
import { columnLabel, noPlanNotice, pathLabel } from '../plan'
import PlanTree from './PlanTree.vue'

const props = defineProps<{ plans: PlanReport[]; notice: string; loading: boolean }>()

/// The reports that have something to draw. The console shows one panel, and a
/// batch with several statements is rare enough that the first is the useful
/// one; the rest are still listed below their own report.
const first = computed<PlanReport | null>(() => props.plans[0] ?? null)
</script>

<template>
  <section v-if="notice" class="plan">
    <el-alert type="info" show-icon :closable="false" title="没有执行计划" :description="notice" />
  </section>

  <section v-else-if="first" class="plan">
    <p v-if="first.chosen" class="chosen">
      <span class="tag">选定路径</span>
      <span class="value">{{ pathLabel(first.chosen) }}</span>
    </p>

    <div v-if="first.rejected.length" class="rejected">
      <span class="tag">否决候选</span>
      <ul>
        <li v-for="(candidate, index) in first.rejected" :key="index">
          <span class="path">{{ candidate.path }}</span>
          <span class="reason">{{ candidate.reason }}</span>
        </li>
      </ul>
    </div>

    <template v-if="first.plan">
      <div class="trees">
        <div>
          <h4>真实算子树<span class="hint">（执行的就是这棵）</span></h4>
          <PlanTree :node="first.plan" />
        </div>
        <div v-if="first.explain">
          <h4>引擎的 EXPLAIN 文本<span class="hint">（同一次判定的文字版）</span></h4>
          <pre class="explain">{{ first.explain }}</pre>
        </div>
      </div>
    </template>
    <el-alert
      v-else
      type="warning"
      show-icon
      :closable="false"
      title="没有算子计划"
      :description="noPlanNotice(first)"
    />

    <template v-if="first.bind">
      <div class="bind">
        <div>
          <h4>表引用</h4>
          <table class="bind-table">
            <thead>
              <tr>
                <th>表</th>
                <th>别名</th>
                <th>引擎</th>
                <th>布局</th>
                <th>文件号</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="table in first.bind.tables" :key="table.name">
                <td>{{ table.name }}</td>
                <td>{{ table.alias ?? '—' }}</td>
                <td>{{ table.engine ?? '—' }}</td>
                <td>{{ table.layout ?? '—' }}</td>
                <td>{{ table.file_no ?? '—' }}</td>
              </tr>
            </tbody>
          </table>
        </div>
        <div>
          <h4>列引用</h4>
          <table class="bind-table">
            <thead>
              <tr>
                <th>写法</th>
                <th>解析结果</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="column in first.bind.columns" :key="column.ref">
                <td>{{ column.ref }}</td>
                <td :class="{ unresolved: column.source !== 'base' }">{{ columnLabel(column) }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </template>
  </section>

  <el-empty v-else-if="loading" description="正在构建计划…" :image-size="60" />
</template>

<style scoped>
.plan {
  display: flex;
  flex-direction: column;
  gap: 12px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
  padding: 12px;
  background: var(--el-bg-color);
}

.tag {
  display: inline-block;
  min-width: 64px;
  color: var(--el-text-color-secondary);
  font-size: 12px;
}

.chosen .value {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}

.rejected ul {
  margin: 4px 0 0;
  padding-left: 0;
  list-style: none;
}

.rejected li {
  display: flex;
  gap: 8px;
  font-size: 12px;
}

.rejected .path {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  color: var(--el-text-color-regular);
  min-width: 130px;
}

.rejected .reason {
  color: var(--el-text-color-secondary);
}

.trees {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 20px;
  align-items: start;
}

h4 {
  margin: 0 0 6px;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

.hint {
  margin-left: 4px;
  font-weight: 400;
  font-size: 11px;
  color: var(--el-text-color-secondary);
}

.explain {
  margin: 0;
  font-size: 12px;
  white-space: pre-wrap;
  word-break: break-all;
  color: var(--el-text-color-secondary);
}

.bind {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 20px;
  align-items: start;
}

.bind-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.bind-table th,
.bind-table td {
  text-align: left;
  padding: 2px 8px 2px 0;
  border-bottom: 1px solid var(--el-border-color-lighter);
}

.bind-table th {
  color: var(--el-text-color-secondary);
  font-weight: 500;
}

/* An unresolved reference is shown as a problem, not left blank. */
.unresolved {
  color: var(--el-color-warning);
}

@media (max-width: 1100px) {
  .trees,
  .bind {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
