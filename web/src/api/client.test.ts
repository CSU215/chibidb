import { beforeEach, describe, expect, it, vi } from 'vitest'

import { ApiError, SESSION_HEADER, ensureSession, parseSql, query, sessionId } from './client'

/// A minimal `sessionStorage`. The tests run in the node environment, and the
/// real thing is a browser global.
class MemoryStorage {
  private entries = new Map<string, string>()

  getItem(key: string): string | null {
    return this.entries.get(key) ?? null
  }

  setItem(key: string, value: string): void {
    this.entries.set(key, value)
  }
}

function reply(body: unknown, init: { status?: number; headers?: Record<string, string> } = {}) {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: { 'Content-Type': 'application/json', ...init.headers },
  })
}

/// The calls the client made, so a test can assert on the request as well as the
/// result.
function stubFetch(...responses: Response[]) {
  const calls: { url: string; init: RequestInit }[] = []
  const mock = vi.fn(async (url: string, init: RequestInit = {}) => {
    calls.push({ url, init })
    const next = responses.shift()
    if (!next) throw new Error('the test did not queue enough responses')
    return next
  })
  vi.stubGlobal('fetch', mock)
  return calls
}

function sentHeader(init: RequestInit): string | null {
  return new Headers(init.headers).get(SESSION_HEADER)
}

beforeEach(() => {
  vi.unstubAllGlobals()
  vi.stubGlobal('sessionStorage', new MemoryStorage())
})

describe('query', () => {
  it('sends the session header and remembers an id the server hands back', async () => {
    sessionStorage.setItem('chibidb.session', 'old-id')
    const calls = stubFetch(
      reply({ results: [] }, { headers: { [SESSION_HEADER]: 'new-id' } }),
    )

    await query('select 1;')

    expect(calls[0].url).toBe('/query')
    expect(sentHeader(calls[0].init)).toBe('old-id')
    // A replacement id has to be adopted, or the next request would keep
    // presenting one the server no longer knows.
    expect(sessionId()).toBe('new-id')
  })

  it('sends no session header before one has been issued', async () => {
    const calls = stubFetch(reply({ results: [] }))

    await query('select 1;')

    expect(sentHeader(calls[0].init)).toBeNull()
  })

  it('returns the results on success', async () => {
    stubFetch(reply({ results: [{ type: 'message', message: 'SUCCESS' }] }))

    await expect(query('create table t (id int);')).resolves.toEqual([
      { type: 'message', message: 'SUCCESS' },
    ])
  })

  it('throws the engine message when the statement fails', async () => {
    stubFetch(reply({ error: 'no such column: from' }, { status: 400 }))

    const failure = await query('select from;').catch((e: unknown) => e)

    expect(failure).toBeInstanceOf(ApiError)
    expect((failure as ApiError).message).toBe('no such column: from')
    expect((failure as ApiError).status).toBe(400)
  })
})

describe('ensureSession', () => {
  it('reuses the id already in storage without asking the server', async () => {
    sessionStorage.setItem('chibidb.session', 'known')
    const calls = stubFetch()

    await expect(ensureSession()).resolves.toBe('known')
    expect(calls).toHaveLength(0)
  })

  it('mints one and stores it when there is none', async () => {
    stubFetch(reply({ session: 'fresh' }, { headers: { [SESSION_HEADER]: 'fresh' } }))

    await expect(ensureSession()).resolves.toBe('fresh')
    expect(sessionId()).toBe('fresh')
  })

  it('fails loudly when the server sends no id', async () => {
    stubFetch(reply({}, { status: 200 }))

    await expect(ensureSession()).rejects.toBeInstanceOf(ApiError)
  })
})

describe('parseSql', () => {
  it('returns the trace', async () => {
    const trace = {
      sql: 'select 1;',
      tokens: [{ kind: 'Ident', text: 'select', pos: 0 }],
      statements: [{ debug: 'Select(..)' }],
      error: null,
    }
    stubFetch(reply(trace))

    await expect(parseSql('select 1;')).resolves.toEqual(trace)
  })

  it('treats a SQL error as data, not as a thrown failure', async () => {
    // The engine answers 200 with `error` set, because this endpoint is asked
    // about text that is still being typed. Throwing here would turn a normal
    // diagnosis into an exception in the UI.
    stubFetch(
      reply({
        sql: 'select 1 +;',
        tokens: [],
        statements: [],
        error: { stage: 'parse', message: 'syntax error: expected expression' },
      }),
    )

    const trace = await parseSql('select 1 +;')
    expect(trace.error?.stage).toBe('parse')
  })

  it('explains a 404 as the admin API being switched off', async () => {
    stubFetch(reply({ error: 'not found' }, { status: 404 }))

    const failure = await parseSql('select 1;').catch((e: unknown) => e)

    expect(failure).toBeInstanceOf(ApiError)
    expect((failure as ApiError).message).toContain('server.admin_api')
  })

  it('surfaces a malformed request as an error', async () => {
    stubFetch(reply({ error: 'expected a string field "sql"' }, { status: 400 }))

    await expect(parseSql('select 1;')).rejects.toThrow('expected a string field')
  })
})
