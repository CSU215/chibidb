<script setup lang="ts">
import { onMounted, ref } from 'vue'

import { query } from '../api/client'
import type { ResultSet } from '../api/types'

const emit = defineEmits<{ selected: [database: string] }>()

const databases = ref<string[]>([])
const current = ref('')
const tables = ref<string[]>([])
const columns = ref<{ name: string; type: string; extra: string }[]>([])
const error = ref('')
const loading = ref(false)

/// Names come from the engine's own listings, so they are not user input -- but
/// they are interpolated into SQL, and a guard is cheaper than reasoning about
/// whether a table name could ever carry a statement separator.
function ident(name: string): string {
  if (!/^[A-Za-z0-9_]+$/.test(name)) throw new Error(`非法标识符: ${name}`)
  return name
}

function textRows(results: ResultSet[]): string[][] {
  const first = results.find((r) => r.type === 'rows')
  if (!first || first.type !== 'rows') return []
  return first.rows.map((row) => row.map((cell) => (cell === null ? '' : String(cell))))
}

async function run(sql: string): Promise<string[][]> {
  return textRows(await query(sql))
}

async function loadDatabases(): Promise<void> {
  loading.value = true
  error.value = ''
  try {
    databases.value = (await run('show databases;')).map((row) => row[0])
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

/// Selecting a database really does run `use`, so the console's statements land
/// in it too -- the tree and the editor share one session, and pretending
/// otherwise would make the header lie about which database is selected.
async function selectDatabase(name: string): Promise<void> {
  loading.value = true
  error.value = ''
  try {
    await query(`use ${ident(name)};`)
    current.value = name
    tables.value = (await run('show tables;')).map((row) => row[0])
    columns.value = []
    emit('selected', name)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

async function selectTable(name: string): Promise<void> {
  loading.value = true
  error.value = ''
  try {
    const rows = await run(`show columns from ${ident(name)};`)
    // `SHOW COLUMNS` returns one column per row; the extra fields differ by
    // engine, so read positionally and degrade rather than assuming a shape.
    columns.value = rows.map((row) => ({
      name: row[0] ?? '',
      type: row[1] ?? '',
      extra: row.slice(2).join(' '),
    }))
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(loadDatabases)
defineExpose({ loadDatabases })
</script>

<template>
  <div class="schema">
    <div class="head">
      <span class="title">数据库</span>
      <el-button link size="small" @click="loadDatabases">刷新</el-button>
    </div>
    <el-alert v-if="error" :title="error" type="error" :closable="false" show-icon />

    <el-radio-group v-model="current" @change="selectDatabase(String(current))">
      <el-radio v-for="name in databases" :key="name" :value="name" class="db">
        {{ name }}
      </el-radio>
    </el-radio-group>

    <template v-if="current">
      <div class="head">
        <span class="title">表 / 视图</span>
      </div>
      <el-empty v-if="!tables.length" description="没有表" :image-size="48" />
      <div v-else class="tables">
        <el-button
          v-for="name in tables"
          :key="name"
          size="small"
          class="table"
          @click="selectTable(name)"
        >
          {{ name }}
        </el-button>
      </div>
    </template>

    <template v-if="columns.length">
      <div class="head"><span class="title">列</span></div>
      <el-table :data="columns" size="small" border max-height="30vh">
        <el-table-column prop="name" label="列名" min-width="90" />
        <el-table-column prop="type" label="类型" min-width="70" />
        <el-table-column prop="extra" label="约束" min-width="90" />
      </el-table>
    </template>
  </div>
</template>

<style scoped>
.schema {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.title {
  font-weight: 600;
  font-size: 13px;
  color: var(--el-text-color-regular);
}

.db {
  width: 100%;
  margin-right: 0;
}

.tables {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.table {
  margin-left: 0;
}
</style>
