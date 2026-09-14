import { createRouter, createWebHashHistory } from 'vue-router'

import PipelineLab from './views/PipelineLab.vue'
import SqlConsole from './views/SqlConsole.vue'

/// Hash history on purpose.
///
/// The engine serves files straight off disk, so a path like `/pipeline` would
/// be a 404 unless the server also grew an SPA fallback route -- which is one
/// more way to get path handling wrong, and one more reason to worry about
/// traversal. With a fragment the server only ever sees `/` and `/assets/*`.
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: '/', name: 'sql', component: SqlConsole },
    { path: '/pipeline', name: 'pipeline', component: PipelineLab },
  ],
})
