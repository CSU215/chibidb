<script setup lang="ts">
import { sql as sqlLanguage } from '@codemirror/lang-sql'
import { lintGutter, linter, type Diagnostic } from '@codemirror/lint'
import { EditorState, Prec } from '@codemirror/state'
import { EditorView, keymap } from '@codemirror/view'
import { basicSetup } from 'codemirror'
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

const props = defineProps<{
  modelValue: string
  /// Runs the compiler and says what to underline. Supplied by the caller so the
  /// editor stays ignorant of HTTP and of where diagnostics come from.
  diagnose: (text: string) => Promise<Diagnostic[]>
  /// How long to wait after the last keystroke before diagnosing.
  delay?: number
}>()

const emit = defineEmits<{
  'update:modelValue': [text: string]
  run: []
}>()

const host = ref<HTMLDivElement | null>(null)
let view: EditorView | null = null

onMounted(() => {
  if (!host.value) return
  view = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        // `Prec.high` because the default keymap already claims Mod-Enter for
        // `insertBlankLine`; without it this binding would never fire.
        Prec.high(
          keymap.of([
            {
              key: 'Mod-Enter',
              run: () => {
                emit('run')
                return true
              },
            },
          ]),
        ),
        basicSetup,
        sqlLanguage(),
        lintGutter(),
        // Debounced here rather than in the caller: `linter` also cancels a
        // stale run when the document changes again, which hand-rolled
        // debouncing would have to reimplement.
        linter((v) => props.diagnose(v.state.doc.toString()), { delay: props.delay ?? 300 }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) emit('update:modelValue', update.state.doc.toString())
        }),
        EditorView.theme({
          '&': { height: '100%', fontSize: '13px' },
          '.cm-scroller': { fontFamily: 'ui-monospace, SFMono-Regular, Menlo, monospace' },
        }),
      ],
    }),
  })
})

watch(
  () => props.modelValue,
  (text) => {
    // Only when the change came from outside. Dispatching unconditionally would
    // fight the user's own typing, and resetting the cursor on every keystroke.
    if (view && text !== view.state.doc.toString()) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } })
    }
  },
)

onBeforeUnmount(() => view?.destroy())
</script>

<template>
  <div ref="host" class="editor" />
</template>

<style scoped>
.editor {
  height: 220px;
  border: 1px solid var(--el-border-color);
  border-radius: 6px;
  overflow: hidden;
}

.editor:focus-within {
  border-color: var(--el-color-primary);
}
</style>
