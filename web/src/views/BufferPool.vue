<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'

import { ApiError, metrics as fetchMetrics, poolEvents, poolFrames } from '../api/client'
import type { FrameView, Metrics, PoolEvent } from '../api/types'
import {
  eventDetail,
  eventLabel,
  eventTone,
  formatBytes,
  formatRate,
  intervalCounters,
  intervalHitRate,
  sparkline,
} from '../pool'

/// Sampling periods. The pool is watched, not driven: a second is fast enough
/// to see what a workload did and slow enough to cost nothing. Events come
/// faster than the counters because an eviction is the thing being read.
const METRICS_MS = 1000
const EVENTS_MS = 500
/// Rate samples the chart keeps (two minutes at 1 Hz).
const HISTORY = 120
/// Events kept on screen; the engine's ring holds 4096.
const LOG_LIMIT = 200

const latest = ref<Metrics | null>(null)
const previous = ref<Metrics | null>(null)
const history = ref<(number | null)[]>([])
const frames = ref<FrameView[]>([])
const events = ref<PoolEvent[]>([])
/// The `seq` of the newest event already seen; the server resumes from it.
const cursor = ref(0)
const truncated = ref(false)
const notice = ref('')
const running = ref(true)
/// Whether the log shows the frames the replacer looked at and left alone.
const showSkipped = ref(true)

let metricsTimer: number | undefined
let eventsTimer: number | undefined

const rate = computed(() => (latest.value ? intervalHitRate(previous.value, latest.value) : null))
const counters = computed(() =>
  latest.value ? intervalCounters(previous.value, latest.value) : null,
)
const chart = computed(() => sparkline(history.value, 100, 100))
const walPercent = computed(() => {
  const wal = latest.value?.wal
  if (!wal || wal.threshold === 0) return 0
  return Math.min(100, (wal.bytes / wal.threshold) * 100)
})
const visibleEvents = computed(() =>
  showSkipped.value ? events.value : events.value.filter((e) => e.kind !== 'evict_skipped'),
)

function describe(error: unknown): string {
  if (error instanceof ApiError) return error.message
  return error instanceof Error ? error.message : String(error)
}

async function sample(): Promise<void> {
  try {
    const next = await fetchMetrics()
    // The previous sample is what a rate is measured against, so it is kept
    // rather than the history being recomputed from scratch.
    const before = latest.value
    previous.value = before
    latest.value = next
    history.value = [...history.value, intervalHitRate(before, next)].slice(-HISTORY)
    notice.value = ''
    void refreshFrames()
  } catch (error) {
    notice.value = describe(error)
  }
}

async function refreshFrames(): Promise<void> {
  try {
    frames.value = (await poolFrames()).frames
  } catch (error) {
    notice.value = describe(error)
  }
}

async function drainEvents(): Promise<void> {
  try {
    const log = await poolEvents(cursor.value)
    cursor.value = log.next
    truncated.value = log.truncated
    if (log.events.length) {
      // Newest first, and the panel keeps only what a person can read.
      events.value = [...log.events.reverse(), ...events.value].slice(0, LOG_LIMIT)
    }
  } catch (error) {
    notice.value = describe(error)
  }
}

function start(): void {
  if (metricsTimer === undefined) {
    metricsTimer = window.setInterval(sample, METRICS_MS)
    void sample()
  }
  if (eventsTimer === undefined) {
    eventsTimer = window.setInterval(drainEvents, EVENTS_MS)
    void drainEvents()
  }
}

function stop(): void {
  window.clearInterval(metricsTimer)
  window.clearInterval(eventsTimer)
  metricsTimer = undefined
  eventsTimer = undefined
}

function toggle(): void {
  running.value = !running.value
  if (running.value) start()
  else stop()
}

/// Forgetting the log is a client-side act: the cursor stays where it is, so
/// the next drain resumes rather than replaying what was cleared.
function clearLog(): void {
  events.value = []
  truncated.value = false
}

/// A hidden tab should not poll -- nobody is reading it, and every poll costs
/// a request on a thread-per-connection server.
function onVisibility(): void {
  if (document.hidden) stop()
  else if (running.value) start()
}

onMounted(() => {
  document.addEventListener('visibilitychange', onVisibility)
  start()
})

onUnmounted(() => {
  stop()
  document.removeEventListener('visibilitychange', onVisibility)
})
</script>

<template>
  <div class="pool">
    <el-alert
      v-if="notice"
      type="warning"
      show-icon
      :closable="false"
      title="缓冲池指标不可用"
      :description="notice"
    />

    <div class="bar">
      <el-button size="small" @click="toggle">{{ running ? '暂停' : '继续' }}轮询</el-button>
      <el-button size="small" @click="clearLog">清空日志</el-button>
      <el-checkbox v-model="showSkipped" label="显示被跳过的帧" />
      <span class="spacer" />
      <span class="hint">每 {{ METRICS_MS / 1000 }}s 采样 · 事件每 {{ EVENTS_MS }}ms</span>
    </div>

    <div class="tiles">
      <div class="tile">
        <span class="label">区间命中率</span>
        <span class="value">{{ formatRate(rate) }}</span>
        <span class="sub">
          本次窗口 {{ counters ? `${counters.hits} 命中 / ${counters.misses} 缺页` : '等待第二次采样' }}
        </span>
      </div>
      <div class="tile">
        <span class="label">常驻帧</span>
        <span class="value">{{ latest ? `${latest.pool.resident} / ${latest.pool.capacity}` : '—' }}</span>
        <span class="sub">容量来自 storage.buffer_pool_frames</span>
      </div>
      <div class="tile">
        <span class="label">累计淘汰</span>
        <span class="value">{{ latest?.pool.evictions ?? '—' }}</span>
        <span class="sub">其中脏换出 {{ latest?.pool.dirty_evictions ?? '—' }}</span>
      </div>
      <div class="tile">
        <span class="label">WAL</span>
        <span class="value">{{ latest ? formatBytes(latest.wal.bytes) : '—' }}</span>
        <span class="sub">阈值 {{ latest ? formatBytes(latest.wal.threshold) : '—' }}</span>
        <el-progress :percentage="walPercent" :show-text="false" :stroke-width="4" />
      </div>
    </div>

    <el-card shadow="never">
      <template #header>
        <span>命中率走势</span>
        <span class="hint">（每次采样一个点；无查找的窗口留空）</span>
      </template>
      <svg class="chart" viewBox="0 0 100 100" preserveAspectRatio="none">
        <line x1="0" y1="0" x2="100" y2="0" class="grid" />
        <line x1="0" y1="50" x2="100" y2="50" class="grid" />
        <line x1="0" y1="100" x2="100" y2="100" class="grid" />
        <path v-if="chart" :d="chart" class="line" />
      </svg>
      <p v-if="!chart" class="empty">攒够两个采样点后开始画线。</p>
    </el-card>

    <el-card shadow="never">
      <template #header>
        <span>常驻帧</span>
        <span class="hint">（{{ frames.length }} 个；pins &gt; 0 的帧不会被淘汰）</span>
      </template>
      <el-table :data="frames" size="small" max-height="240">
        <el-table-column prop="file" label="文件" width="80" />
        <el-table-column prop="page" label="页" width="80" />
        <el-table-column prop="pins" label="pin" width="80" />
        <el-table-column label="脏" width="80">
          <template #default="{ row }">
            <el-tag v-if="row.dirty" type="warning" size="small">脏</el-tag>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="引用位" width="100">
          <template #default="{ row }">
            <el-tag v-if="row.accessed" type="info" size="small">已引用</el-tag>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-card v-if="latest?.lsm.length" shadow="never">
      <template #header><span>LSM 表</span></template>
      <el-table :data="latest.lsm" size="small">
        <el-table-column prop="table" label="表" />
        <el-table-column label="各级 SSTable">
          <template #default="{ row }">{{ row.levels.join(' / ') }}</template>
        </el-table-column>
        <el-table-column label="memtable">
          <template #default="{ row }">{{ formatBytes(row.memtable_bytes) }}</template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-card shadow="never">
      <template #header>
        <span>替换日志</span>
        <span class="hint">
          （最新在前，游标 {{ cursor }}；只显示最近 {{ LOG_LIMIT }} 条）
        </span>
      </template>
      <el-alert
        v-if="truncated"
        type="info"
        show-icon
        :closable="false"
        title="日志被追上了一圈"
        description="环只有 4096 个位置，这段时间里的事件已经滚出去了 —— 下面是环里还留着的部分。"
      />
      <el-table :data="visibleEvents" size="small" max-height="360">
        <el-table-column prop="seq" label="#" width="80" />
        <el-table-column label="事件" width="100">
          <template #default="{ row }">
            <el-tag :type="eventTone(row.kind)" size="small">{{ eventLabel(row.kind) }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="帧">
          <template #default="{ row }">{{ eventDetail(row) }}</template>
        </el-table-column>
      </el-table>
      <p v-if="!visibleEvents.length" class="empty">
        还没有事件。跑一条查询，缺页、淘汰与回写都会出现在这里。
      </p>
    </el-card>
  </div>
</template>

<style scoped>
.pool {
  display: flex;
  flex-direction: column;
  gap: 12px;
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
  margin-left: 4px;
  font-weight: 400;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.tiles {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 12px;
}

.tile {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 12px;
  border: 1px solid var(--el-border-color-lighter);
  border-radius: 6px;
}

.tile .label {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.tile .value {
  font-size: 22px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}

.tile .sub {
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.chart {
  width: 100%;
  height: 120px;
  background: var(--el-fill-color-lighter);
}

.grid {
  stroke: var(--el-border-color-lighter);
  stroke-width: 0.5;
}

.line {
  fill: none;
  stroke: var(--el-color-primary);
  stroke-width: 1.2;
  vector-effect: non-scaling-stroke;
}

.empty {
  margin: 8px 0 0;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.muted {
  color: var(--el-text-color-secondary);
}

@media (max-width: 900px) {
  .tiles {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}
</style>
