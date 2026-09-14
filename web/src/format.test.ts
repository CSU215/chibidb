import { describe, expect, it } from 'vitest'

import { display, ident, shortId } from './format'

describe('display', () => {
  it('keeps NULL distinguishable from the empty string and from its own name', () => {
    expect(display(null)).toBe('NULL')
    expect(display('')).toBe('')
    expect(display('NULL')).toBe('NULL')
    // The first two must not collide: one is SQL NULL, the other an empty value.
    expect(display(null)).not.toBe(display(''))
  })

  it('does not pretend a boolean is a number', () => {
    expect(display(true)).toBe('true')
    expect(display(false)).toBe('false')
    expect(display(0)).toBe('0')
  })

  it('passes numbers and strings through', () => {
    expect(display(42)).toBe('42')
    expect(display(-1.5)).toBe('-1.5')
    expect(display('alice')).toBe('alice')
  })
})

describe('ident', () => {
  it('accepts the names the engine hands out', () => {
    expect(ident('shop')).toBe('shop')
    expect(ident('t_2')).toBe('t_2')
  })

  it('refuses anything that could carry SQL', () => {
    for (const name of ['t; drop table x', 't--', "t'", 'a b', '', 't.x', '语句']) {
      expect(() => ident(name), name).toThrow()
    }
  })
})

describe('shortId', () => {
  it('truncates a long id and handles the empty case', () => {
    expect(shortId('0123456789abcdef')).toBe('01234567')
    expect(shortId('0123456789abcdef', 4)).toBe('0123')
    expect(shortId('')).toBe('…')
  })
})
