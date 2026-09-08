use crate::storage::disk::DiskManager;

pub struct BufferPool {
    #[allow(dead_code)]
    disk: DiskManager,
}

impl BufferPool {
    pub fn new(disk: DiskManager) -> Self {
        Self { disk }
    }
}
