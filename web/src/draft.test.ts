import { beforeEach, describe, expect, it, vi } from 'vitest'

import { loadDraft, saveDraft } from './draft'

class MemoryStorage {
  private entries = new Map<string, string>()

  getItem(key: string): string | null {
    return this.entries.get(key) ?? null
  }

  setItem(key: string, value: string): void {
    this.entries.set(key, value)
  }
}

/// Storage that throws on every access, as private modes and some policies do.
const hostileStorage = {
  getItem(): string {
    throw new Error('storage disabled')
  },
  setItem(): void {
    throw new Error('storage disabled')
  },
}

beforeEach(() => {
  vi.unstubAllGlobals()
})

describe('draft', () => {
  it('falls back when nothing has been saved', () => {
    vi.stubGlobal('sessionStorage', new MemoryStorage())
    expect(loadDraft('k', 'sample')).toBe('sample')
  })

  it('round-trips what the editor held', () => {
    vi.stubGlobal('sessionStorage', new MemoryStorage())

    saveDraft('k', 'select * from t;')

    expect(loadDraft('k', 'sample')).toBe('select * from t;')
  })

  it('keeps an empty draft distinct from no draft', () => {
    vi.stubGlobal('sessionStorage', new MemoryStorage())

    // Deleting everything is a choice the user made, and reloading must not
    // put the sample back under their cursor.
    saveDraft('k', '')

    expect(loadDraft('k', 'sample')).toBe('')
  })

  it('keeps drafts apart per key', () => {
    vi.stubGlobal('sessionStorage', new MemoryStorage())

    saveDraft('sql', 'select 1;')
    saveDraft('pipeline', 'select 2;')

    expect(loadDraft('sql', '')).toBe('select 1;')
    expect(loadDraft('pipeline', '')).toBe('select 2;')
  })

  it('degrades instead of throwing when storage is unavailable', () => {
    vi.stubGlobal('sessionStorage', hostileStorage)

    // The editor must still open and still accept typing; it simply will not
    // remember anything.
    expect(() => saveDraft('k', 'text')).not.toThrow()
    expect(loadDraft('k', 'sample')).toBe('sample')
  })
})
