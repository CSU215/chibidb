import { describe, expect, it } from 'vitest'

import type { BindColumn, PlanNode, PlanPath, PlanReport } from './api/types'
import { columnLabel, detailLine, noPlanNotice, pathLabel, sourceLabel } from './plan'

const node = (name: string, detail: PlanNode['detail'] = [], children: PlanNode[] = []): PlanNode => ({
  node: name,
  detail,
  children,
})

const report = (over: Partial<PlanReport> = {}): PlanReport => ({
  explain: null,
  chosen: null,
  rejected: [],
  physical: true,
  plan: node('TableScan'),
  bind: null,
  error: null,
  ...over,
})

describe('detailLine', () => {
  it('joins the pairs the engine sent, in the order it sent them', () => {
    expect(detailLine(node('IndexScan', [{ key: 'table', value: 't' }, { key: 'column', value: 'id' }]))).toBe(
      'table=t · column=id',
    )
  })

  it('is empty for an operator with nothing to say', () => {
    expect(detailLine(node('ConstantScan'))).toBe('')
  })
})

describe('pathLabel', () => {
  it('names the index and the column it drives', () => {
    const chosen: PlanPath = {
      path: 'IndexScan',
      table: 't',
      index: 'idx_id',
      column: 'id',
      condition: 'id > 5',
    }
    expect(pathLabel(chosen)).toBe('IndexScan idx_id(id) on t · where id > 5')
  })

  it('does not print fields that do not apply', () => {
    const scan: PlanPath = {
      path: 'FullScan',
      table: 't',
      index: null,
      column: null,
      condition: null,
    }
    expect(pathLabel(scan)).toBe('FullScan t')
    expect(pathLabel(scan)).not.toContain('null')
  })

  it('handles a join, which has no table of its own', () => {
    const join: PlanPath = { path: 'HashJoin', table: null, index: null, column: null, condition: null }
    expect(pathLabel(join)).toBe('HashJoin')
  })
})

describe('noPlanNotice', () => {
  it('says nothing when there is a tree to draw', () => {
    expect(noPlanNotice(report())).toBe('')
  })

  it('explains a statement the operator layer does not cover', () => {
    const notice = noPlanNotice(report({ plan: null, physical: false }))
    expect(notice).toContain('物化执行器')
  })

  it('prefers the engine message when there is one', () => {
    const notice = noPlanNotice(report({ plan: null, error: { stage: 'plan', message: 'no such table: x' } }))
    expect(notice).toBe('no such table: x')
  })

  it('never answers with an empty string for a report that has no plan', () => {
    expect(noPlanNotice(report({ plan: null })).length).toBeGreaterThan(0)
  })
})

describe('columnLabel', () => {
  const column = (over: Partial<BindColumn>): BindColumn => ({
    ref: 'id',
    qualifier: null,
    table: 't',
    column: 'id',
    type: 'int',
    source: 'base',
    ...over,
  })

  it('names the base column and its type', () => {
    expect(columnLabel(column({}))).toBe('t.id : int')
  })

  it('does not invent a table for a reference that did not resolve', () => {
    const unresolved = columnLabel(column({ table: null, column: null, type: null, source: 'unknown' }))
    expect(unresolved).toBe(sourceLabel('unknown'))
    expect(unresolved).not.toContain('null')
  })

  it('keeps an ambiguous reference visibly ambiguous', () => {
    expect(columnLabel(column({ source: 'ambiguous', table: null, column: null }))).toContain('歧义')
  })
})
