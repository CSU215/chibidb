import type {
  FrameView,
  Metrics,
  ParseTrace,
  PlanTrace,
  PoolEventLog,
  ResultSet,
} from './types'

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

/// `/session` always answers 200 with the header, so any other status means the
/// request did not get an answer from the engine.
///
/// This message matters more than it looks: the common cause is not a broken
/// session but a request that never arrived -- nothing listening on the port the
/// console is pointed at, because `server.http_addr` is unset, so the dev
/// server's proxy answers 500 with an empty body. Saying "the server did not
/// hand out a session" for that sends the reader looking in the wrong place.
function sessionFailure(status: number, detail: string): string {
  if (status >= 500) {
    return (
      `无法连接引擎（HTTP ${status}）：请求没有到达服务端。` +
      `请确认后端已启动，且 config.toml 里设了 server.http_addr（当前代理目标见 vite.config.ts）。` +
      (detail ? ` ${detail}` : '')
    )
  }
  return `服务端没有下发会话（HTTP ${status}）。`
}

/// Fetches an id if this tab does not have one yet.
export async function ensureSession(): Promise<string> {
  const existing = sessionId()
  if (existing) return existing

  const response = await fetch('/session')
  const id = response.headers.get(SESSION_HEADER)
  if (!id) {
    // A proxy's error body is plain text, not JSON, so read it defensively.
    const detail = await response.text().catch(() => '')
    throw new ApiError(sessionFailure(response.status, detail.trim().slice(0, 200)), response.status)
  }
  remember(id)
  return id
}

/// The admin endpoints answer 404 when `server.admin_api` is off, and that is
/// the one failure worth naming: everything else is a transport problem.
function adminFailure(path: string, status: number): ApiError {
  if (status === 404) {
    return new ApiError(
      '内省接口未开启。在服务端 config.toml 里设 server.admin_api = true 后重启。',
      404,
    )
  }
  return new ApiError(`${path} 请求失败（HTTP ${status}）`, status)
}

async function adminGet<T>(path: string): Promise<T> {
  const response = await send(path)
  if (!response.ok) throw adminFailure(path, response.status)
  return (await payload(response)) as unknown as T
}

/// Pool counters, the WAL's position and the LSM tables. Polled on a timer.
export async function metrics(): Promise<Metrics> {
  return adminGet<Metrics>('/api/metrics')
}

/// The frames resident right now.
export async function poolFrames(): Promise<{ frames: FrameView[] }> {
  return adminGet<{ frames: FrameView[] }>('/api/bufferpool/frames')
}

/// The event log after `since` (the `seq` of the last event already seen).
export async function poolEvents(since: number): Promise<PoolEventLog> {
  return adminGet<PoolEventLog>(`/api/bufferpool/events?since=${since}`)
}

/// Asks for the plan without running the statement.
///
/// Building a plan is not free for the engine -- an index scan resolves its row
/// ids up front -- so this is called when a statement is run or when the plan
/// panel is opened, not on every keystroke.
///
/// Like `/api/parse`, a SQL problem is not thrown: it arrives as `trace.error`
/// (or per statement, as `plans[].error`) on a 200.
export async function planSql(sql: string): Promise<PlanTrace> {
  const response = await send('/api/plan', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ sql }),
  })
  if (response.status === 404) {
    throw new ApiError(
      '计划接口未开启。在服务端 config.toml 里设 server.admin_api = true 后重启。',
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
  return body as unknown as PlanTrace
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
