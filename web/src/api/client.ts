import type { ParseTrace, ResultSet } from './types'

const SESSION_KEY = 'chibidb.session'

/// The session header the engine keeps sessions under. Requests that carry it
/// share one server-side session; requests without it are per-connection, which
/// is why the console always sends it.
export const SESSION_HEADER = 'X-Chibi-Session'

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

/// `sessionStorage` rather than `localStorage`, on purpose: it is per tab, and
/// two tabs sharing one id would serialise on the server's session lock and
/// fight over the selected database.
export function sessionId(): string | undefined {
  return sessionStorage.getItem(SESSION_KEY) ?? undefined
}

function remember(id: string | null): void {
  if (id) sessionStorage.setItem(SESSION_KEY, id)
}

/// Attaches the session header and records whatever id comes back. The engine
/// hands out a replacement when the one we sent is not known (a restart, or an
/// expired session), so a stale id self-heals rather than erroring.
async function send(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers)
  const id = sessionId()
  if (id) headers.set(SESSION_HEADER, id)

  const response = await fetch(path, { ...init, headers })
  remember(response.headers.get(SESSION_HEADER))
  return response
}

/// Reads the JSON body, turning a transport-level failure into an ApiError.
async function payload(response: Response): Promise<Record<string, unknown>> {
  try {
    return (await response.json()) as Record<string, unknown>
  } catch {
    throw new ApiError(`unreadable response (HTTP ${response.status})`, response.status)
  }
}

/// Runs one SQL statement batch against whatever database the session has
/// selected. Throws `ApiError` when the engine reports one.
export async function query(sql: string): Promise<ResultSet[]> {
  const response = await send('/query', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sql }),
  })
  const body = await payload(response)
  if (typeof body.error === 'string') throw new ApiError(body.error, response.status)
  if (!response.ok) throw new ApiError(`request failed (HTTP ${response.status})`, response.status)
  return (body.results ?? []) as ResultSet[]
}

/// Fetches an id if this tab does not have one yet.
export async function ensureSession(): Promise<string> {
  const existing = sessionId()
  if (existing) return existing

  const response = await fetch('/session')
  const id = response.headers.get(SESSION_HEADER)
  if (!id) {
    throw new ApiError(`the server did not hand out a session (HTTP ${response.status})`, response.status)
  }
  remember(id)
  return id
}

/// Compiles the SQL without running it.
///
/// A SQL error is not thrown: it arrives as `trace.error` on a 200, because this
/// endpoint is asked about text that is still being typed. Only a failure of the
/// request itself throws.
export async function parseSql(sql: string): Promise<ParseTrace> {
  const response = await send('/api/parse', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sql }),
  })
  if (response.status === 404) {
    throw new ApiError(
      '解析接口未开启。在服务端 config.toml 里设 server.admin_api = true 后重启。',
      404,
    )
  }
  const body = await payload(response)
  if (!response.ok) {
    throw new ApiError(
      typeof body.error === 'string' ? body.error : `request failed (HTTP ${response.status})`,
      response.status,
    )
  }
  return body as unknown as ParseTrace
}
