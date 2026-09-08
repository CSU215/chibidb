pub const PAGE_SIZE: usize = 8192;

pub type PageNo = u32;

pub type FileId = u32;

pub type PageData = Box<[u8; PAGE_SIZE]>;

pub fn zeroed_page() -> PageData {
    Box::new([0u8; PAGE_SIZE])
}
