import { createRouter, createWebHashHistory } from 'vue-router'

import BufferPool from './views/BufferPool.vue'
import SqlConsole from './views/SqlConsole.vue'

/// Hash history on purpose.
///
/// The engine serves files straight off disk, so a path like `/pipeline` would
/// be a 404 unless the server also grew an SPA fallback route -- which is one
/// more way to get path handling wrong, and one more reason to worry about
/// traversal. With a fragment the server only ever sees `/` and `/assets/*`.
///
/// Two routes, split by what they are about rather than by size: the console is
/// about a statement (write it, run it, read its diagnosis and its plan), while
/// the pool panel is about the engine's own state. The latter polls, so it gets
/// a page that can be left -- leaving it stops the polling.
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: '/', name: 'sql', component: SqlConsole },
    { path: '/bufferpool', name: 'bufferpool', component: BufferPool },
  ],
})
