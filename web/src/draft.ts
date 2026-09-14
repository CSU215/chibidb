/// Remembers an editor's text per browser tab.
///
/// `sessionStorage` rather than `localStorage`, matching where the session id
/// lives: a draft belongs to this tab's session, and two tabs sharing one key
/// would overwrite each other on every keystroke.
///
/// Storage can also be unavailable outright -- private modes and some policy
/// settings make `sessionStorage` throw on access rather than return null. A
/// console that refuses to open because it cannot remember a draft would be a
/// worse trade, so every failure here degrades to "not remembered".
export function loadDraft(key: string, fallback: string): string {
  try {
    return sessionStorage.getItem(key) ?? fallback
  } catch {
    return fallback
  }
}

export function saveDraft(key: string, text: string): void {
  try {
    sessionStorage.setItem(key, text)
  } catch {
    // Nothing to do: the editor keeps working, the draft just will not survive
    // a reload.
  }
}
