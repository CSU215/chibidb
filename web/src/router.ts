import { createRouter, createWebHashHistory } from 'vue-router'

import SqlConsole from './views/SqlConsole.vue'

/// Hash history on purpose.
///
/// The engine serves files straight off disk, so a path like `/pipeline` would
/// be a 404 unless the server also grew an SPA fallback route -- which is one
/// more way to get path handling wrong, and one more reason to worry about
/// traversal. With a fragment the server only ever sees `/` and `/assets/*`.
///
/// One route: the compiler trace lives in a panel on the console rather than on
/// a page of its own, so that a statement is written once and its diagnosis sits
/// next to its results.
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [{ path: '/', name: 'sql', component: SqlConsole }],
})
