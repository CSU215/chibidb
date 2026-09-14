use crate::storage::page::PAGE_SIZE;
use crate::{Error, Result};

/// On-disk format version, bumped whenever a file layout changes.
pub const FORMAT_VERSION: u16 = 1;

/// Length of the common header prefix: magic(8) + version(2) + kind(1) +
/// page_size(4).
pub const HEADER_LEN: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Heap,
    Index,
    Catalog,
}

impl FileKind {
    fn as_byte(self) -> u8 {
        match self {
            FileKind::Heap => 0,
            FileKind::Index => 1,
            FileKind::Catalog => 2,
        }
    }

    fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(FileKind::Heap),
            1 => Ok(FileKind::Index),
            2 => Ok(FileKind::Catalog),
            other => Err(Error::Runtime(format!("unknown file kind {other}"))),
        }
    }
}

/// Writes the common header at the start of `page`.
pub fn write_header(page: &mut [u8], magic: &[u8; 8], kind: FileKind) {
    page[0..8].copy_from_slice(magic);
    page[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page[10] = kind.as_byte();
    page[11..15].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
}

/// Validates the common header against the expected type. Pages created with a
/// different format version, kind or page size are rejected so callers do not
/// silently misread them.
pub fn read_header(page: &[u8], magic: &[u8; 8], kind: FileKind) -> Result<()> {
    if page.len() < HEADER_LEN || &page[0..8] != magic {
        return Err(Error::Runtime("not a chaoticdb data file".into()));
    }
    let version = u16::from_le_bytes(page[8..10].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(Error::Runtime(format!(
            "unsupported file format version {version} (expected {FORMAT_VERSION})"
        )));
    }
    let file_kind = FileKind::from_byte(page[10])?;
    if file_kind != kind {
        return Err(Error::Runtime("file kind does not match its contents".into()));
    }
    let page_size = u32::from_le_bytes(page[11..15].try_into().unwrap());
    if page_size as usize != PAGE_SIZE {
        return Err(Error::Runtime(format!(
            "file page size {page_size} does not match build page size {PAGE_SIZE}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: [u8; 8] = *b"CHIDTEST";

    #[test]
    fn roundtrip_and_kind_mismatch() {
        let mut page = [0u8; 64];
        write_header(&mut page, &MAGIC, FileKind::Heap);
        assert!(read_header(&page, &MAGIC, FileKind::Heap).is_ok());
        assert!(read_header(&page, &MAGIC, FileKind::Index).is_err());
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut page = [0u8; 64];
        write_header(&mut page, &MAGIC, FileKind::Index);
        assert!(read_header(&page, b"CHIDOTHR", FileKind::Index).is_err());
    }

    #[test]
    fn rejects_wrong_version() {
        let mut page = [0u8; 64];
        write_header(&mut page, &MAGIC, FileKind::Heap);
        page[8] = 99;
        assert!(read_header(&page, &MAGIC, FileKind::Heap).is_err());
    }

    #[test]
    fn rejects_wrong_page_size() {
        let mut page = [0u8; 64];
        write_header(&mut page, &MAGIC, FileKind::Heap);
        page[11..15].copy_from_slice(&4096u32.to_le_bytes());
        assert!(read_header(&page, &MAGIC, FileKind::Heap).is_err());
    }

    #[test]
    fn rejects_short_input() {
        assert!(read_header(&[0u8; 4], &MAGIC, FileKind::Heap).is_err());
    }
}
