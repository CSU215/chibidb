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

export type ParseError = { stage: ParseStage; message: string }

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
