<script setup lang="ts">
import { onMounted } from 'vue'

import { shortId } from './format'
import { useSession } from './stores/session'

const session = useSession()

onMounted(session.bootstrap)
</script>

<template>
  <el-container class="app">
    <el-header class="header">
      <div class="brand">
        <strong>chibidb</strong>
        <span class="subtitle">控制台</span>
      </div>
      <nav class="nav">
        <router-link to="/">SQL 控制台</router-link>
        <router-link to="/bufferpool">缓冲池</router-link>
      </nav>
      <div class="spacer" />
      <div class="session">
        <el-tag size="small" type="info">会话 {{ shortId(session.id) }}</el-tag>
        <el-tag v-if="session.currentDb" size="small">{{ session.currentDb }}</el-tag>
      </div>
    </el-header>

    <el-main>
      <el-alert
        v-if="session.error"
        type="error"
        show-icon
        :closable="false"
        title="无法建立会话"
        :description="session.error"
      />
      <router-view v-else />
    </el-main>
  </el-container>
</template>

<style scoped>
.app {
  min-height: 100vh;
}

.header {
  display: flex;
  align-items: center;
  gap: 24px;
  border-bottom: 1px solid var(--el-border-color-lighter);
  padding: 0 20px;
}

.brand {
  display: flex;
  align-items: baseline;
  gap: 6px;
  font-size: 16px;
}

.subtitle {
  color: var(--el-text-color-secondary);
  font-size: 12px;
}

.nav {
  display: flex;
  gap: 16px;
  font-size: 13px;
}

.nav a {
  color: var(--el-text-color-regular);
  text-decoration: none;
  padding: 2px 0;
  border-bottom: 2px solid transparent;
}

.nav a.router-link-active {
  color: var(--el-color-primary);
  border-bottom-color: var(--el-color-primary);
}

.spacer {
  flex: 1;
}

.session {
  display: flex;
  gap: 8px;
}
</style>
