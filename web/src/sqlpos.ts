import type { ParseError } from './api/types'

/// Converts a UTF-8 byte offset into a UTF-16 string index.
///
/// The engine counts bytes and the editor counts UTF-16 code units, and the two
/// only agree while the text is ASCII. Any non-ASCII character before the error
/// -- a Chinese string literal, say -- shifts every later position, so the
/// conversion is not optional.
export function byteToCharIndex(text: string, byteOffset: number): number {
  const encoder = new TextEncoder()
  let bytes = 0
  let index = 0
  // Iterating a string yields code points, and `length` gives the code units
  // each one occupies -- which is what an editor position is measured in.
  for (const char of text) {
    if (bytes >= byteOffset) break
    bytes += encoder.encode(char).length
    index += char.length
  }
  return index
}

/// The span to underline for an error reported at `byteOffset`.
///
/// The engine reports a start offset only, so the end is inferred: a word run, a
/// quoted run, or a single character.
export function markerSpan(text: string, byteOffset: number): { from: number; to: number } {
  const from = byteToCharIndex(text, byteOffset)
  // Nothing to underline past the end of the text, and a zero-width marker is
  // invisible -- so blame the last character instead. "Something is missing
  // here" reads better pointed at what exists than at nothing.
  if (from >= text.length) {
    const last = Math.max(0, text.length - 1)
    return { from: last, to: text.length }
  }

  const rest = text.slice(from)
  const quoted = /^(['"])(?:(?!\1)[^\\]|\\.)*\1?/.exec(rest)
  if (quoted) {
    return { from, to: from + quoted[0].length }
  }
  const word = /^[A-Za-z0-9_]+/.exec(rest)
  if (word) {
    return { from, to: from + word[0].length }
  }
  return { from, to: from + 1 }
}

/// The single position an error should be drawn at, or null when it has none.
///
/// `message` is what the bubble shows; `from`/`to` are the underline.
export type Marker = { from: number; to: number; message: string }

export function markerFor(text: string, error: ParseError): Marker | null {
  // A null position is not "byte zero": runtime complaints have nowhere to
  // point, and drawing them at the start of the text would be a lie.
  if (error.pos === null) return null
  const span = markerSpan(text, error.pos)
  return { ...span, message: error.message }
}
