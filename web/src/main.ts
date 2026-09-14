import ElementPlus from 'element-plus'
import { createPinia } from 'pinia'
import { createApp } from 'vue'

import 'element-plus/dist/index.css'

import App from './App.vue'
import { router } from './router'

// Element Plus is imported whole rather than per component: this is a local
// console served off the same origin as the engine, so a few hundred KB of CSS
// and JS costs less than the build-time machinery to tree-shake it.
createApp(App).use(createPinia()).use(router).use(ElementPlus).mount('#app')
