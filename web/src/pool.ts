import type { Metrics, PoolEvent } from './api/types'

/// What changed between two cumulative samples.
export type Counters = { hits: number; misses: number; evictions: number }

/// The counts between two samples. `null` for the first sample, which has
/// nothing to subtract from.
///
/// The engine reports cumulative counters on purpose (no window on its side),
/// so the interval -- and therefore the rate -- is entirely the reader's
/// choice. Clamping at zero keeps a pool that was reset (a reopened database)
/// from showing a negative rate.
export function intervalCounters(previous: Metrics | null, current: Metrics): Counters | null {
  if (!previous) return null
  return {
    hits: Math.max(0, current.pool.hits - previous.pool.hits),
    misses: Math.max(0, current.pool.misses - previous.pool.misses),
    evictions: Math.max(0, current.pool.evictions - previous.pool.evictions),
  }
}

/// The share of the interval's lookups that were served from cache.
///
/// `null` -- not 0 -- when the interval had no lookups at all: an idle pool did
/// not "miss everything", it did nothing, and drawing it as 0% would be a lie
/// the chart then repeats.
export function intervalHitRate(previous: Metrics | null, current: Metrics): number | null {
  const counters = intervalCounters(previous, current)
  if (!counters) return null
  const lookups = counters.hits + counters.misses
  return lookups === 0 ? null : counters.hits / lookups
}

export function formatRate(rate: number | null): string {
  return rate === null ? '—' : `${(rate * 100).toFixed(1)}%`
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(2)} MB`
}

/// A polyline path through `values` (each in 0..1), oldest first, for an SVG
/// viewBox of `width` x `height`. Gaps (`null`) are skipped rather than drawn
/// as breaks: the panel is a trend, not a signal.
export function sparkline(values: (number | null)[], width: number, height: number): string {
  const points = values
    .map((value, index) => ({
      value,
      x: (index / Math.max(1, values.length - 1)) * width,
    }))
    .filter((point): point is { value: number; x: number } => point.value !== null)
  if (points.length < 2) return ''
  return points
    .map((point, index) => {
      const y = height - point.value * height
      return `${index === 0 ? 'M' : 'L'} ${point.x.toFixed(1)} ${y.toFixed(1)}`
    })
    .join(' ')
}

const LABELS: Record<PoolEvent['kind'], string> = {
  load: '载入',
  evict: '淘汰',
  evict_skipped: '跳过',
  flush: '回写',
  discard: '丢弃',
}

export function eventLabel(kind: PoolEvent['kind']): string {
  return LABELS[kind] ?? kind
}

/// A one-line description of an event, including the one field that says *why*
/// a frame was left alone: a skipped frame with `pins > 0` was in use, one with
/// `pins == 0` got a second chance from the CLOCK hand.
export function eventDetail(event: PoolEvent): string {
  const where = `file=${event.file} page=${event.page}`
  switch (event.kind) {
    case 'evict':
      return `${where} ${event.dirty ? '脏' : '干净'}`
    case 'evict_skipped':
      return `${where} ${event.pins > 0 ? `被 pin（${event.pins}）` : '第二次机会'}`
    case 'load':
      return where
    case 'flush':
      return `${where} 已回写`
    case 'discard':
      return `${where} 未回写`
  }
}

/// Element Plus tag colours, so the log reads as states rather than as text.
export function eventTone(kind: PoolEvent['kind']): 'primary' | 'warning' | 'info' | 'danger' {
  switch (kind) {
    case 'evict':
      return 'warning'
    case 'evict_skipped':
      return 'danger'
    case 'flush':
      return 'primary'
    default:
      return 'info'
  }
}
