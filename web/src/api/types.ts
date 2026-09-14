/// A column value as the engine serialises it. `null` is SQL NULL and must stay
/// distinguishable from the empty string.
export type Cell = null | boolean | number | string

/// The envelope `POST /query` returns, one entry per statement.
export type ResultSet =
  | { type: 'message'; message: string }
  | { type: 'rows'; columns: string[]; rows: Cell[][] }

/// Which compiler stage rejected the SQL. The lexer is all-or-nothing, so a
/// `lex` failure has no token stream to show.
export type ParseStage = 'lex' | 'parse'

export type ParseError = {
  stage: ParseStage
  message: string
  /// UTF-8 byte offset of the offending token, or null when the engine has no
  /// position to give -- runtime complaints like `no such column` have none,
  /// and are shown in a banner rather than marked in the editor.
  pos: number | null
}

export type Token = { kind: string; text: string; pos: number }

/// `POST /api/parse`: the SQL as the compiler sees it, without running it.
/// `error` is non-null when the SQL does not compile, and the response is still
/// HTTP 200 -- see the endpoint's own docs for why.
export type ParseTrace = {
  sql: string
  tokens: Token[]
  statements: { debug: string }[]
  error: ParseError | null
}

/// One operator of the tree that will actually run. `detail` is ordered, and
/// every value is already a string: the engine never asks the console to
/// interpret a number.
export type PlanNode = {
  node: string
  detail: { key: string; value: string }[]
  children: PlanNode[]
}

/// The access path the planner recorded. Kept apart from the tree above on
/// purpose: this is the planner's own record of a decision, the tree is what
/// runs, and the two are cross-checked by the engine's tests.
export type PlanPath = {
  path: string
  table: string | null
  index: string | null
  column: string | null
  condition: string | null
}

export type RejectedPath = { path: string; reason: string }

export type BindTable = {
  name: string
  alias: string | null
  database: string
  engine: string | null
  layout: string | null
  file_no: number | null
}

/// How a column reference resolved. Read `source` first: `unknown` and
/// `ambiguous` mean the name did *not* resolve, and the other fields are null.
export type BindSource = 'base' | 'alias' | 'ambiguous' | 'unknown'

export type BindColumn = {
  ref: string
  qualifier: string | null
  table: string | null
  column: string | null
  type: string | null
  source: BindSource
}

export type BindInfo = { tables: BindTable[]; columns: BindColumn[] }

/// What the console shows about one statement: the tree that runs, why that
/// path was chosen, and what the names resolved to.
export type PlanReport = {
  explain: string | null
  chosen: PlanPath | null
  rejected: RejectedPath[]
  /// False when the operator layer does not cover the statement, so `plan` is
  /// null and the row-at-a-time executor runs it instead.
  physical: boolean
  plan: PlanNode | null
  bind: BindInfo | null
  error: { stage: string; message: string } | null
}

/// `POST /api/plan`: one report per statement. A statement with no plan is a
/// result, not an error -- the response is still HTTP 200.
export type PlanTrace = {
  sql: string
  database: string
  error: ParseError | null
  plans: PlanReport[]
}

/// Cumulative pool counters. A rate is the reader's subtraction: the engine
/// keeps no window, so changing the polling interval changes nothing here.
export type PoolCounters = {
  capacity: number
  resident: number
  hits: number
  misses: number
  evictions: number
  dirty_evictions: number
  hit_rate: number
}

export type LsmTable = { table: string; levels: number[]; memtable_bytes: number }

/// `GET /api/metrics`.
export type Metrics = {
  database: string
  pool: PoolCounters
  wal: { bytes: number; threshold: number }
  lsm: LsmTable[]
}

/// One resident frame. `accessed` is the CLOCK reference bit.
export type FrameView = {
  file: number
  page: number
  pins: number
  dirty: boolean
  accessed: boolean
}

/// What happened to one frame. `file`/`page` name the frame the event is
/// about: for `evict` that is the victim, for `evict_skipped` a frame the
/// replacer examined and left alone.
export type PoolEventKind = 'load' | 'evict' | 'evict_skipped' | 'flush' | 'discard'

export type PoolEvent = {
  seq: number
  kind: PoolEventKind
  file: number
  page: number
  pins: number
  dirty: boolean
}

/// `GET /api/bufferpool/events?since=N`. `next` is the cursor to send back;
/// `truncated` means the reader fell behind the ring and missed events.
export type PoolEventLog = {
  events: PoolEvent[]
  next: number
  truncated: boolean
}
