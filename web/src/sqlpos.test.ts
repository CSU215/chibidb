import { describe, expect, it } from 'vitest'

import type { ParseError } from './api/types'
import { byteToCharIndex, markerFor, markerSpan } from './sqlpos'

const syntaxError = (pos: number | null, message = 'boom'): ParseError => ({
  stage: 'parse',
  message,
  pos,
})

describe('byteToCharIndex', () => {
  it('is the identity for ASCII', () => {
    const text = 'select 1 +;'
    for (let i = 0; i <= text.length; i += 1) {
      expect(byteToCharIndex(text, i)).toBe(i)
    }
  })

  it('accounts for multi-byte characters before the offset', () => {
    // '中' is three UTF-8 bytes but one UTF-16 unit, so byte offsets past it run
    // ahead of editor positions. Without the conversion every marker after a
    // non-ASCII literal would land in the wrong place.
    const text = "select '中' + ;"
    const byteOfSemicolon = new TextEncoder().encode(text).length - 1
    const charOfSemicolon = text.length - 1

    expect(charOfSemicolon).toBeLessThan(byteOfSemicolon)
    expect(byteToCharIndex(text, byteOfSemicolon)).toBe(charOfSemicolon)
  })

  it('counts an astral character as two code units', () => {
    const text = 'a😀b'
    // '😀' is 4 bytes and, in UTF-16, a surrogate pair.
    expect(byteToCharIndex(text, 5)).toBe(3)
  })

  it('clamps past the end rather than overrunning', () => {
    expect(byteToCharIndex('abc', 99)).toBe(3)
  })
})

describe('markerSpan', () => {
  it('underlines the whole token that starts at the offset', () => {
    const text = 'select 9999;'
    expect(markerSpan(text, 7)).toEqual({ from: 7, to: 11 })
  })

  it('underlines a quoted run, including an unterminated one', () => {
    expect(markerSpan("select 'abc'", 7)).toEqual({ from: 7, to: 12 })
    expect(markerSpan("select 'abc", 7)).toEqual({ from: 7, to: 11 })
  })

  it('underlines a single punctuation character', () => {
    const text = 'select 1 +;'
    expect(markerSpan(text, 9)).toEqual({ from: 9, to: 10 })
  })

  it('blames the last character when the offset is past the end', () => {
    // A zero-width marker at the end would be invisible, and "something is
    // missing here" reads better pointed at what does exist.
    const text = 'create table t (id int'
    expect(markerSpan(text, text.length)).toEqual({ from: text.length - 1, to: text.length })
  })

  it('does not overrun an empty document', () => {
    expect(markerSpan('', 0)).toEqual({ from: 0, to: 0 })
  })
})

describe('markerFor', () => {
  it('carries the message with the span', () => {
    expect(markerFor('select 1 +;', syntaxError(9, 'expected an expression'))).toEqual({
      from: 9,
      to: 10,
      message: 'expected an expression',
    })
  })

  it('returns null when the engine reported no position', () => {
    // Byte zero is a real position, so null must not be treated as zero --
    // that would draw a marker at the start of an unrelated statement.
    expect(markerFor('select from;', syntaxError(null))).toBeNull()
  })
})
