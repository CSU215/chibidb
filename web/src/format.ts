import type { Cell } from './api/types'

/// Renders a cell for the grid.
///
/// NULL must be visibly different from the empty string -- and from the literal
/// text "NULL" -- or the grid misreports what is actually in the database. The
/// same goes for booleans: showing `1` for true would be a lie about the value's
/// type.
export function display(cell: Cell): string {
  if (cell === null) return 'NULL'
  if (cell === true) return 'true'
  if (cell === false) return 'false'
  return String(cell)
}

/// Guards a name before it is interpolated into SQL.
///
/// Names come from the engine's own listings rather than from user input, so
/// this is belt and braces; but a guard is cheaper than reasoning about whether
/// a table name could ever carry a statement separator.
export function ident(name: string): string {
  if (!/^[A-Za-z0-9_]+$/.test(name)) throw new Error(`非法标识符: ${name}`)
  return name
}

/// The first N characters of a session id, for the header badge. Long ids are
/// unreadable, and the badge only has to tell two tabs apart.
export function shortId(id: string, length = 8): string {
  return id ? id.slice(0, length) : '…'
}
