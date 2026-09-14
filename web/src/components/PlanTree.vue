<script setup lang="ts">
import type { PlanNode } from '../api/types'
import { detailLine } from '../plan'

/// One operator of the tree that runs, drawn with whatever it said about
/// itself. The component recurses over `children`, so a new operator shows up
/// here without any change to the console: the engine's own `children()`
/// decides the shape.
defineProps<{ node: PlanNode }>()
</script>

<template>
  <div class="node">
    <div class="row">
      <span class="name">{{ node.node }}</span>
      <span v-if="detailLine(node)" class="detail">{{ detailLine(node) }}</span>
    </div>
    <div v-if="node.children.length" class="children">
      <PlanTree v-for="(child, index) in node.children" :key="index" :node="child" />
    </div>
  </div>
</template>

<style scoped>
.node {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  line-height: 1.7;
}

.row {
  display: flex;
  gap: 8px;
  align-items: baseline;
  flex-wrap: wrap;
}

.name {
  font-weight: 600;
  color: var(--el-color-primary);
}

.detail {
  color: var(--el-text-color-secondary);
}

/* A guide line rather than indentation on the row itself: it makes the depth
   readable when a node wraps onto a second line. */
.children {
  margin-left: 8px;
  padding-left: 12px;
  border-left: 1px dashed var(--el-border-color);
}
</style>
