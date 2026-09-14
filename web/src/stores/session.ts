import { defineStore } from 'pinia'
import { ref } from 'vue'

import { ensureSession, sessionId } from '../api/client'

/// The console's view of the server-side session.
///
/// The id itself lives in `sessionStorage` (see `api/client.ts`); this store
/// mirrors the parts the UI has to render -- the id badge and the selected
/// database -- and keeps them current as statements change them.
export const useSession = defineStore('session', () => {
  const id = ref(sessionId() ?? '')
  const currentDb = ref('')
  const ready = ref(false)
  const error = ref('')

  async function bootstrap(): Promise<void> {
    try {
      id.value = await ensureSession()
      error.value = ''
      ready.value = true
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      ready.value = false
    }
  }

  /// Called after a `use <db>` so the header reflects the session's database.
  function setCurrentDb(name: string): void {
    currentDb.value = name
  }

  /// Mirrors an id the server replaced for us (unknown or expired), so the badge
  /// does not keep showing one that no longer names anything.
  function refreshId(): void {
    id.value = sessionId() ?? id.value
  }

  return { id, currentDb, ready, error, bootstrap, setCurrentDb, refreshId }
})
