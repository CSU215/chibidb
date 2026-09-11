use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::storage::page::{PageNo, PAGE_SIZE};
use crate::{Error, Result};

fn io_err(e: std::io::Error) -> Error {
    Error::Runtime(format!("double-write buffer io error: {e}"))
}

/// A staging area for pages about to be written to their final location.
///
/// A page is first appended to the double-write file and synced, then written
/// to its final file. If the process dies mid-write, recovery replays the
/// staged page so a torn final write is repaired. The buffer is truncated once
/// every staged page has reached its final location.
pub struct DoubleWrite {
    file: File,
}

impl DoubleWrite {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_err)?;
        Ok(Self { file })
    }

    /// Appends `page` for `target` at page number `no`.
    pub fn stage(&mut self, target: &Path, no: PageNo, page: &[u8]) -> Result<()> {
        let path = target.to_string_lossy();
        let bytes = path.as_bytes();
        self.file.seek(SeekFrom::End(0)).map_err(io_err)?;
        self.file
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .map_err(io_err)?;
        self.file.write_all(bytes).map_err(io_err)?;
        self.file.write_all(&no.to_le_bytes()).map_err(io_err)?;
        self.file.write_all(page).map_err(io_err)?;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data().map_err(io_err)
    }

    pub fn reset(&mut self) -> Result<()> {
        self.file.set_len(0).map_err(io_err)?;
        self.file.seek(SeekFrom::Start(0)).map_err(io_err)?;
        Ok(())
    }
}

/// Replays every complete staged page via `apply`, then truncates the buffer.
/// A truncated trailing record (crash mid-append) is ignored.
pub fn recover(
    path: &Path,
    mut apply: impl FnMut(&Path, PageNo, &[u8]) -> Result<()>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read(path).map_err(io_err)?;
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let path_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let record = 4 + path_len + 4 + PAGE_SIZE;
        if pos + record > data.len() {
            break;
        }
        let target = std::str::from_utf8(&data[pos + 4..pos + 4 + path_len])
            .map_err(|e| Error::Runtime(format!("corrupt double-write path: {e}")))?;
        let no = u32::from_le_bytes(
            data[pos + 4 + path_len..pos + 4 + path_len + 4].try_into().unwrap(),
        );
        let page = &data[pos + 4 + path_len + 4..pos + record];
        apply(Path::new(target), no, page)?;
        pos += record;
    }
    // every recoverable page has been applied; drop the buffer
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_err)?
        .set_len(0)
        .map_err(io_err)
}

/// Writes one page into `target` at `no`, creating the file if absent and
/// zero-filling any gap so the file stays page-aligned.
pub fn write_page_at(target: &Path, no: PageNo, page: &[u8]) -> Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(target) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(e)),
    };
    let offset = no as u64 * PAGE_SIZE as u64;
    let len = file.metadata().map_err(io_err)?.len();
    if offset > len {
        file.seek(SeekFrom::Start(len)).map_err(io_err)?;
        let gap = [0u8; PAGE_SIZE];
        let mut pos = len;
        while pos < offset {
            let n = PAGE_SIZE.min((offset - pos) as usize);
            file.write_all(&gap[..n]).map_err(io_err)?;
            pos += n as u64;
        }
    }
    file.seek(SeekFrom::Start(offset)).map_err(io_err)?;
    file.write_all(page).map_err(io_err)?;
    file.flush().map_err(io_err)
}

/// Reads a file fully, used only by tests.
#[cfg(test)]
fn read_all(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut f = File::open(path).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_and_recover_repairs_target() {
        let dir = tempfile::tempdir().unwrap();
        let dwb_path = dir.path().join("dwb.bin");
        let target = dir.path().join("t.dbf");

        // target exists with two zeroed pages
        std::fs::write(&target, vec![0u8; PAGE_SIZE * 2]).unwrap();

        let mut dwb = DoubleWrite::open(&dwb_path).unwrap();
        let page = vec![7u8; PAGE_SIZE];
        dwb.stage(&target, 1, &page).unwrap();
        dwb.sync().unwrap();
        // simulate a crash: the final write never happened

        recover(&dwb_path, write_page_at).unwrap();

        let bytes = read_all(&target);
        assert_eq!(&bytes[PAGE_SIZE..PAGE_SIZE * 2], &page[..]);
        // the buffer is truncated after recovery
        assert_eq!(std::fs::metadata(&dwb_path).unwrap().len(), 0);
    }

    #[test]
    fn truncated_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let dwb_path = dir.path().join("dwb.bin");
        let target = dir.path().join("t.dbf");
        std::fs::write(&target, vec![0u8; PAGE_SIZE]).unwrap();

        let mut dwb = DoubleWrite::open(&dwb_path).unwrap();
        dwb.stage(&target, 0, &vec![9u8; PAGE_SIZE]).unwrap();
        dwb.sync().unwrap();

        // append a partial record
        let mut f = OpenOptions::new().append(true).open(&dwb_path).unwrap();
        f.write_all(&[1, 2, 3]).unwrap();
        drop(f);

        recover(&dwb_path, write_page_at).unwrap();
        let bytes = read_all(&target);
        assert_eq!(&bytes[0..PAGE_SIZE], &vec![9u8; PAGE_SIZE][..]);
    }
}
