import { describe, expect, it } from 'vitest'

import type { Metrics, PoolEvent } from './api/types'
import {
  eventDetail,
  eventLabel,
  explainInterval,
  formatBytes,
  intervalCounters,
  intervalHitRate,
  sparkline,
} from './pool'

const metrics = (over: Partial<Metrics['pool']> = {}, wal = { bytes: 0, threshold: 100 }): Metrics => ({
  database: 'main',
  pool: {
    capacity: 64,
    resident: 10,
    hits: 0,
    misses: 0,
    evictions: 0,
    dirty_evictions: 0,
    hit_rate: 0,
    ...over,
  },
  wal,
  lsm: [],
})

describe('intervalCounters', () => {
  it('subtracts two cumulative samples', () => {
    const before = metrics({ hits: 100, misses: 10, evictions: 3 })
    const after = metrics({ hits: 180, misses: 30, evictions: 5 })
    expect(intervalCounters(before, after)).toEqual({ hits: 80, misses: 20, evictions: 2 })
  })

  it('has nothing to report for the first sample', () => {
    expect(intervalCounters(null, metrics({ hits: 5 }))).toBeNull()
  })

  it('does not go negative when the pool was reset', () => {
    // A reopened database starts its counters over; a negative "rate" would be
    // worse than a zero one.
    const before = metrics({ hits: 500 })
    const after = metrics({ hits: 3 })
    expect(intervalCounters(before, after)?.hits).toBe(0)
  })
})

describe('intervalHitRate', () => {
  it('is the share of lookups served from cache', () => {
    const before = metrics({ hits: 0, misses: 0 })
    const after = metrics({ hits: 75, misses: 25 })
    expect(intervalHitRate(before, after)).toBeCloseTo(0.75)
  })

  it('is null, not zero, for an interval with no lookups', () => {
    // An idle pool did not miss everything; drawing it as 0% would be a lie.
    const sample = metrics({ hits: 42, misses: 8 })
    expect(intervalHitRate(sample, sample)).toBeNull()
  })

  it('is null for the first sample', () => {
    expect(intervalHitRate(null, metrics({ hits: 5 }))).toBeNull()
  })
})

describe('sparkline', () => {
  it('maps the first sample to the left edge and the last to the right', () => {
    const path = sparkline([0, 1], 100, 10)
    expect(path.startsWith('M 0.0 10.0')).toBe(true)
    expect(path.endsWith('L 100.0 0.0')).toBe(true)
  })

  it('inverts the y axis: a full cache sits at the top', () => {
    expect(sparkline([1, 1], 10, 10)).toContain('0.0')
    expect(sparkline([0, 0], 10, 10)).toContain('10.0')
  })

  it('is empty when there is nothing to draw', () => {
    expect(sparkline([], 10, 10)).toBe('')
    expect(sparkline([0.5], 10, 10)).toBe('')
    expect(sparkline([null, null], 10, 10)).toBe('')
  })

  it('skips gaps instead of drawing a line through them', () => {
    const path = sparkline([0, null, 1], 100, 10)
    expect(path).not.toContain('NaN')
    expect(path.split('L').length).toBe(2)
  })
})

describe('eventDetail', () => {
  const event = (over: Partial<PoolEvent>): PoolEvent => ({
    seq: 1,
    kind: 'load',
    file: 0,
    page: 7,
    pins: 0,
    dirty: false,
    ...over,
  })

  it('says why a frame was left alone', () => {
    // The two reasons are different problems: one is in use, the other just
    // got a second chance.
    expect(eventDetail(event({ kind: 'evict_skipped', pins: 2 }))).toContain('被 pin')
    expect(eventDetail(event({ kind: 'evict_skipped', pins: 0 }))).toContain('第二次机会')
  })

  it('reports the victim and whether it needed writing back', () => {
    expect(eventDetail(event({ kind: 'evict', dirty: true }))).toContain('脏')
    expect(eventDetail(event({ kind: 'evict', dirty: false }))).toContain('干净')
  })

  it('names every kind', () => {
    for (const kind of ['load', 'evict', 'evict_skipped', 'flush', 'discard'] as const) {
      expect(eventLabel(kind).length).toBeGreaterThan(0)
      expect(eventDetail(event({ kind })).length).toBeGreaterThan(0)
    }
  })
})

describe('formatBytes', () => {
  it('scales the unit to the size', () => {
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(2048)).toBe('2.0 KB')
    expect(formatBytes(8 * 1024 * 1024)).toBe('8.00 MB')
  })
})

describe('explainInterval', () => {
  const counters = (hits: number, misses: number) => ({ hits, misses, evictions: 0 })

  it('says nothing when the rate speaks for itself', () => {
    expect(explainInterval(counters(10, 2), 64)).toBe('')
    expect(explainInterval(null, 64)).toBe('')
  })

  it('names the working set when nothing hit', () => {
    // A flat 0% looks like a broken meter; it is the expected reading for a
    // scan that does not fit, and the pool size is the missing half of that.
    const hint = explainInterval(counters(0, 115), 64)
    expect(hint).toContain('0 次命中')
    expect(hint).toContain('64 帧')
    expect(hint).toContain('512.0 KB')
    expect(hint).toContain('buffer_pool_frames')
  })

  it('distinguishes an idle window from a thrashing one', () => {
    expect(explainInterval(counters(0, 0), 64)).toContain('没有页访问')
    expect(explainInterval(counters(0, 0), 64)).not.toContain('工作集')
  })
})
