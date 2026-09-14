import type { BindColumn, BindSource, PlanNode, PlanPath, PlanReport } from './api/types'

/// A node's detail as one line: `table=t · column=id · access=index lookup`.
///
/// The engine sends the pairs in display order, so this only joins them -- the
/// console never decides what an operator's interesting fields are.
export function detailLine(node: PlanNode): string {
  return node.detail.map((d) => `${d.key}=${d.value}`).join(' · ')
}

/// The chosen access path as one line, e.g. `IndexScan(idx_id) on t · where id > 5`.
///
/// Only the fields that exist are shown: a sequential scan has no index, and
/// printing `index=null` for it would read as a missing value rather than as
/// "not applicable".
export function pathLabel(chosen: PlanPath): string {
  const parts: string[] = []
  if (chosen.index) parts.push(`${chosen.index}(${chosen.column ?? ''})`)
  if (chosen.table) parts.push(chosen.table)
  const head = `${chosen.path}${parts.length ? ` ${parts.join(' on ')}` : ''}`
  return chosen.condition ? `${head} · where ${chosen.condition}` : head
}

/// What to show when there is no tree to draw.
///
/// Never empty for a report with no plan: the panel has to say *why* it is
/// blank, or a reader cannot tell "nothing runs here" from "the console could
/// not ask".
export function noPlanNotice(report: PlanReport): string {
  if (report.plan) return ''
  if (report.error) return report.error.message
  return '这条语句不经过算子层，由物化执行器逐行执行，因此没有算子计划。'
}

/// How a column reference resolved, in the reader's language. `unknown` and
/// `ambiguous` are shown as failures rather than as blanks: a panel that
/// silently displays the wrong column is worse than one that displays none.
export function sourceLabel(source: BindSource): string {
  switch (source) {
    case 'base':
      return '基础列'
    case 'alias':
      return 'SELECT 别名'
    case 'ambiguous':
      return '歧义（多张表都有该列）'
    case 'unknown':
      return '未解析'
  }
}

/// The base column a reference resolved to, or the reason it did not.
export function columnLabel(column: BindColumn): string {
  if (column.source !== 'base') return sourceLabel(column.source)
  return `${column.table}.${column.column} : ${column.type ?? '?'}`
}
