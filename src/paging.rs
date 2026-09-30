use std::{io::{Read, Seek, Write}, sync::Arc};

use crate::util::FileLike;

pub struct PagedFile {
    file: Arc<dyn FileLike>,
    page_size: u64,
}

impl PagedFile {
    pub fn new(file: impl FileLike + 'static, page_size: u64) -> Self {
        assert!(page_size.is_power_of_two(), "page_size must be a power of two");
        assert!(page_size >= 8, "page_size must be at least 8 bytes");
        Self { file: Arc::new(file), page_size }
    }

    pub fn open_existing_block(&self, id: u64) -> std::io::Result<PFileBlockHandle> {
        let mut ph = [0; 8];
        self.file.read_exact_at(&mut ph, id * self.page_size)?;
        let ph = PageHeader::new(u64::from_le_bytes(ph), self.page_size);
        if !ph.is_start() {
            // this page is not the start one, but a continuation of some other page
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Page is not a starting page"));
        }
        Ok(PFileBlockHandle { file: self.file.clone(), current_page: id, current_page_header: ph, prev_page_num: ph.link_none(), pos_in_page: 0, pos_in_block: 0 })
    }

    pub fn create_new_block(&self) -> std::io::Result<(u64, PFileBlockHandle)> {
        let (page_id, page_header) = Self::alloc_new_page(&*self.file, self.page_size, None)?;
        Ok((page_id, PFileBlockHandle { file: self.file.clone(), current_page: page_id, current_page_header: page_header, prev_page_num: page_header.link_none(), pos_in_page: 0, pos_in_block: 0 }))
    }

    fn alloc_new_page(file: &dyn FileLike, page_size: u64, prev: Option<u64>) -> std::io::Result<(u64, PageHeader)> {
        let pos = file.stream_length()?;
        debug_assert!(pos % page_size == 0);
        let page_number = pos / page_size;
        let mut ph = PageHeader::new(0, page_size);
        ph.set_is_start(prev.is_none());
        ph.set_link(prev, None);
        let mut page = vec![0u8; page_size as usize];
        page[0..8].copy_from_slice(&ph.value.to_le_bytes());
        file.write_all_at(&page, pos)?;
        Ok((page_number, ph))
    }

    pub fn open_existing_block_cached(&self, id: u64) -> std::io::Result<PFileBlockCachedHandle> {
        let mut pages = vec![];
        let mut page_i =  id;
        let mut buf= [0; 8];
        let last_page_len = loop {
            self.file.read_exact_at(&mut buf, page_i * self.page_size)?;
            let ph = PageHeader::new(u64::from_le_bytes(buf), self.page_size);
            let prev = if pages.is_empty() {
                if !ph.is_start() {
                    // this page is not the start one, but a continuation of some other page
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Page is not a starting page"));
                }
                ph.link_none()
            } else {
                pages[pages.len() - 1]
            };
            pages.push(page_i);
            match ph.get_next(prev) {
                Some(next) => page_i = next,
                None => { // last page
                    break ph.len()
                }
            }
        };
        Ok(PFileBlockCachedHandle { file: self.file.clone(), pages, last_page_len, curr_page_idx: 0, pos_in_page: 0, page_size: self.page_size })
    }

    pub fn create_new_block_cached(&self) -> std::io::Result<(u64, PFileBlockCachedHandle)> {
        let (page_id, PFileBlockHandle { file, .. } ) = self.create_new_block()?;
        Ok((page_id, PFileBlockCachedHandle { file, pages: vec![page_id], last_page_len: 0, curr_page_idx: 0, pos_in_page: 0, page_size: self.page_size }))
    }

    /// Marks a block, and all its pages, as "garbage", so a future garbage collector
    /// can potentially reuse them. Note that this is not enforced, a user may still access
    /// these blocks, but there are no guarantees that the data will be preserved. 
    pub fn mark_garbage(&self, block_id: u64) -> std::io::Result<()> {
        // mark by setting it to an otherwise invalid state:
        // is_start=true, len=PAGE_SIZE-1, prev=LINK_NONE
        let mut buf = [0; 8];
        self.file.read_exact_at(&mut buf, block_id * self.page_size)?;
        let mut ph = PageHeader::new(u64::from_le_bytes(buf), self.page_size);
        ph.set_is_start(true);
        ph.set_len(self.page_size - 8); // normally, len should be <= PAGE_CAPACITY, so this is invalid
        self.file.write_all_at(&ph.value.to_le_bytes(), block_id * self.page_size)
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Number of usable storage bytes per page, excluding the 8 bytes used for the page header.
    pub fn page_capacity(&self) -> u64 {
        self.page_size - 8
    }
}

#[derive(Clone, Copy)]
struct PageHeader {
    /// bit 0: is_start
    /// bit 1..(PAGE_SIZE_LOG2+1): len
    /// bit (PAGE_SIZE_LOG2+1)..63: link (XOR of prev and next)
    value: u64,
    page_size_log2: u32,
}

impl PageHeader {
    pub fn new(raw_value: u64, page_size: u64) -> Self {
        Self { value: raw_value, page_size_log2: page_size.trailing_zeros() }
    }
    pub fn replace(&mut self, new_value: u64) {
        self.value = new_value;
    }
    pub fn is_start(&self) -> bool {
        self.value & 1 != 0
    }
    pub fn set_is_start(&mut self, is_start: bool) {
        if is_start { self.value |= 1; } else { self.value &= !1; }
    }
    pub fn len(&self) -> u64 {
        (self.value >> 1) & ((1 << self.page_size_log2) - 1)
    }
    pub fn set_len(&mut self, len: u64) {
        debug_assert!(len <= (1 << self.page_size_log2) - 1);
        self.value = (self.value & !(((1 << self.page_size_log2) - 1) << 1)) | (len << 1);
    }
    fn link(&self) -> u64 {
        self.value >> (self.page_size_log2 + 1)
    }
    fn _set_link(&mut self, link: u64) {
        self.value = (self.value & ((1 << (self.page_size_log2 + 1)) - 1)) | (link << (self.page_size_log2 + 1));
    }

    fn link_none(&self) -> u64 {
        (1 << (64 - self.page_size_log2 - 1)) - 1
    }
    fn page_capacity(&self) -> u64 {
        1 << (self.page_size_log2 - 1)
    }
    fn page_size(&self) -> u64 {
        1 << self.page_size_log2
    }

    fn is_full(&self) -> bool {
        debug_assert!(self.len() <= self.page_capacity());
        self.len() >= self.page_capacity()
    }
    fn get_next(&self, prev: u64) -> Option<u64> {
        if !self.is_full() { return None }
        let n = self.link() ^ prev;
        if n == self.link_none() { return None }
        Some(n)
    }
    fn get_prev_from_next(&self, next: u64) -> Option<u64> {
        if self.is_start() { return None }
        let n = self.link() ^ next;
        debug_assert!(n != self.link_none());
        Some(n)
    }
    fn set_link(&mut self, prev: Option<u64>, next: Option<u64>) {
        let prev = if let Some(p) = prev {
            debug_assert!(!self.is_start());
            p
        } else { self.link_none() };
        let next = if let Some(n) = next {
            debug_assert!(self.is_full());
            n
        } else { self.link_none() };
        self._set_link(prev ^ next);
    }
}

// TODO (maybe?): improve performance by loading the current page
// into memory and reading/writing from/to it, instead of doing a io read/write for every call
pub struct PFileBlockHandle {
    file: Arc<dyn FileLike>,
    current_page: u64,
    current_page_header: PageHeader,
    prev_page_num: u64,
    pos_in_page: u64,
    pos_in_block: u64,
}

impl PFileBlockHandle {
    fn page_size(&self) -> u64 { // stored in page header
        self.current_page_header.page_size()
    }
    fn link_none(&self) -> u64 {
        self.current_page_header.link_none()
    }
    fn page_capacity(&self) -> u64 {
        self.current_page_header.page_capacity()
    }

    fn prev(&self) -> Option<u64> {
        if self.current_page_header.is_start() { None }
        else if self.prev_page_num == self.link_none() { None }
        else { Some(self.prev_page_num) }
    }
    fn next(&self) -> Option<u64> {
        self.current_page_header.get_next(self.prev_page_num)
    }

    pub fn dbg(&self) {
        println!("Page {} (pos {}, page len: {}), prev: {:?}, next: {:?}, pos in block: {}",
            self.current_page, self.pos_in_page, self.current_page_header.len(),
            self.prev(), self.next(), self.pos_in_block);
    }
    
    pub fn dup(&self) -> Self {
        Self { file: self.file.clone(), current_page: self.current_page, current_page_header: self.current_page_header, prev_page_num: self.prev_page_num, pos_in_page: self.pos_in_page, pos_in_block: self.pos_in_block }
    }

    /// note: doesn't change `self.pos_in_page`
    fn move_to_prev_page(&mut self) -> std::io::Result<()> {
        debug_assert!(self.prev().is_some());
        let old_page_num = self.current_page;
        let mut ph = [0; 8];
        self.file.read_exact_at(&mut ph, self.prev_page_num * self.page_size())?;
        self.current_page_header.replace(u64::from_le_bytes(ph));
        self.current_page = self.prev_page_num;
        self.prev_page_num = self.current_page_header.get_prev_from_next(old_page_num).unwrap_or(self.link_none());
        Ok(())
    }

    /// note: doesn't change `self.pos_in_page`
    fn move_to_next_page(&mut self) -> std::io::Result<()> {
        debug_assert!(self.current_page_header.is_full());
        let old_page_num = self.current_page;
        let next_page_num = self.current_page_header.get_next(self.prev_page_num).unwrap();
        let mut ph = [0; 8];
        self.file.read_exact_at(&mut ph, next_page_num * self.page_size())?;
        self.current_page_header.replace(u64::from_le_bytes(ph));
        self.current_page = next_page_num;
        self.prev_page_num = old_page_num;
        Ok(())
    }

    fn write_header(&self) -> std::io::Result<()> {
        self.file.write_all_at(&self.current_page_header.value.to_le_bytes(), self.current_page * self.page_size())
    }

    /// Shrink the block so that it ends at the current position
    pub fn shrink(&mut self) -> std::io::Result<()> {
        self.current_page_header.set_len(self.pos_in_page);
        self.current_page_header.set_link(self.prev(), None);
        self.write_header()?;
        if let Some(next_page) = self.next() {
            // similar code to mark_garbage, but also remove the `prev` link
            let mut buf = [0; 8];
            self.file.read_exact_at(&mut buf, next_page * self.page_size())?;
            let mut ph = PageHeader::new(u64::from_le_bytes(buf), self.page_size());
            ph.set_is_start(true);
            ph.set_len(self.page_capacity());
            ph.set_link(None, ph.get_next(self.current_page));
            self.file.write_all_at(&ph.value.to_le_bytes(), next_page * self.page_size())?;
        }
        Ok(())
    }
}

impl Read for PFileBlockHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0;
        loop {
            let readn = (buf.len() - n).min((self.current_page_header.len() - self.pos_in_page) as usize);
            self.file.read_exact_at(&mut buf[n..n+readn], self.current_page * self.page_size() + 8 + self.pos_in_page)?;
            n += readn;
            self.pos_in_page += readn as u64;
            self.pos_in_block += readn as u64;
            if n == buf.len() { return Ok(n) }
            else {
                // need to read more
                debug_assert!(self.pos_in_page == self.current_page_header.len());
                if self.next().is_none() {
                    // there are no more pages, so we can't read more
                    return Ok(n);
                } else {
                    self.move_to_next_page()?;
                    self.pos_in_page = 0;
                }
            }
        }
    }
    
    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let n = self.read(buf)?;
        if n != buf.len() {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Failed to read exact number of bytes"))
        } else {
            Ok(())
        }
    }
}

impl Write for PFileBlockHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut n = 0;
        loop {
            let written = (buf.len() - n).min((self.page_capacity() - self.pos_in_page) as usize);
            self.file.write_all_at(&buf[n..n+written], self.current_page * self.page_size() + 8 + self.pos_in_page)?;
            n += written;
            self.pos_in_page += written as u64;
            if self.pos_in_page > self.current_page_header.len() {
                self.current_page_header.set_len(self.pos_in_page);
                self.write_header()?;
            }
            self.pos_in_block += written as u64;
            if n == buf.len() { return Ok(n) }
            else {
                // need to write more
                debug_assert!(self.pos_in_page == self.page_capacity());
                if self.next().is_some() {
                    // there is a next page, so we can move to it
                    self.move_to_next_page()?;
                    self.pos_in_page = 0;
                } else {
                    // there is no next page, so we need to create one
                    let (new_page_num, new_ph) = PagedFile::alloc_new_page(&*self.file, self.page_size(), Some(self.current_page))?;
                    self.current_page_header.set_link(self.prev(), Some(new_page_num));
                    self.write_header()?;
                    self.prev_page_num = self.current_page;
                    self.current_page_header = new_ph;
                    self.current_page = new_page_num;
                    self.pos_in_page = 0;
                }
            }
        }
        
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
    
    fn write_all(&mut self, mut buf: &[u8]) -> std::io::Result<()> {
        self.write(&mut buf)?;
        Ok(()) // our implementation always writes all bytes
    }
}

impl Seek for PFileBlockHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(n) => {
                self.seek(std::io::SeekFrom::Current(n as i64 - self.pos_in_block as i64))
            },
            std::io::SeekFrom::End(n) => {
                // go to end
                while self.current_page_header.is_full() && self.next().is_some() {
                    self.move_to_next_page()?;
                    self.pos_in_block += self.page_capacity() - self.pos_in_page;
                    self.pos_in_page = 0;
                }
                self.pos_in_block += self.current_page_header.len() - self.pos_in_page;
                self.pos_in_page = self.current_page_header.len();
                self.seek(std::io::SeekFrom::Current(n as i64))
            },
            std::io::SeekFrom::Current(n) => {
                if n >= 0 {
                    let mut remaining = n as u64;
                    while remaining > 0 {
                        let can_move = (self.current_page_header.len() - self.pos_in_page).min(remaining);
                        self.pos_in_page += can_move;
                        self.pos_in_block += can_move;
                        remaining -= can_move;
                        if remaining > 0 {
                            if self.next().is_none() {
                                return Ok(self.pos_in_block);
                            }
                            self.move_to_next_page()?;
                            self.pos_in_page = 0;
                        }
                    }
                    Ok(self.pos_in_block)
                } else {
                    let mut remaining = (-n) as u64;
                    while remaining > 0 {
                        let can_move = self.pos_in_page.min(remaining);
                        self.pos_in_page -= can_move;
                        self.pos_in_block -= can_move;
                        remaining -= can_move;
                        if remaining > 0 {
                            if self.current_page_header.is_start() {
                                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Seek before start of block"));
                            }
                            self.move_to_prev_page()?;
                            self.pos_in_page = self.current_page_header.len();
                        }
                    }
                    Ok(self.pos_in_block)
                }
            },
        }
    }
    
    fn stream_position(&mut self) -> std::io::Result<u64> {
        Ok(self.pos_in_block)
    }
}

/// This handle saves a list of the pages this block contains,
/// greatly increasing performance if the user seeks back and forth a lot,
/// but increasing memory use and startup time.
pub struct PFileBlockCachedHandle {
    file: Arc<dyn FileLike>,
    page_size: u64,
    pages: Vec<u64>,
    last_page_len: u64,
    curr_page_idx: usize, // NB: not current page number, but index inside [`pages`]
    pos_in_page: u64,
}

impl PFileBlockCachedHandle {
    fn page_capacity(&self) -> u64 {
        self.page_size - 8
    }

    fn update_last_page_header(&self, next: Option<u64>) -> std::io::Result<()> {
        debug_assert!(self.curr_page_idx == self.pages.len() - 1);
        let mut ph = PageHeader::new(0, self.page_size);
        let is_start = self.curr_page_idx == 0;
        ph.set_is_start(is_start);
        ph.set_len(self.last_page_len);
        ph.set_link(if is_start { None } else { Some(self.pages[self.curr_page_idx - 1]) }, next);
        self.file.write_all_at(&ph.value.to_le_bytes(), self.pages[self.curr_page_idx] * self.page_size)
    }
}

impl Read for PFileBlockCachedHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0;
        loop {
            let current_page_len = if self.curr_page_idx == self.pages.len() - 1 { self.last_page_len } else { self.page_capacity() }; 
            let readn = (buf.len() - n).min((current_page_len - self.pos_in_page) as usize);
            self.file.read_exact_at(&mut buf[n..n+readn], self.pages[self.curr_page_idx] * self.page_size + 8 + self.pos_in_page)?;
            n += readn;
            self.pos_in_page += readn as u64;
            if n == buf.len() { return Ok(n) }
            else {
                // need to read more
                if self.curr_page_idx == self.pages.len() - 1 {
                    // there are no more pages, so we can't read more
                    return Ok(n);
                } else {
                    self.curr_page_idx += 1;
                    self.pos_in_page = 0;
                }
            }
        }
    }
        
    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let n = self.read(buf)?;
        if n != buf.len() {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Failed to read exact number of bytes"))
        } else {
            Ok(())
        }
    }
}

impl Write for PFileBlockCachedHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut n = 0;
        loop {
            let written = (buf.len() - n).min((self.page_capacity() - self.pos_in_page) as usize);
            self.file.write_all_at(&buf[n..n+written], self.pages[self.curr_page_idx] * self.page_size + 8 + self.pos_in_page)?;
            n += written;
            self.pos_in_page += written as u64;
            if self.curr_page_idx == self.pages.len() - 1 && self.pos_in_page > self.last_page_len {
                self.last_page_len = self.pos_in_page;
                self.update_last_page_header(None)?; // update `len` field
            }
            if n == buf.len() { return Ok(n) }
            else {
                // need to write more
                debug_assert!(self.pos_in_page == self.page_capacity());
                if self.curr_page_idx < self.pages.len() - 1 {
                    self.curr_page_idx += 1;
                    self.pos_in_page = 0;
                } else {
                    // there is no next page, so we need to create one
                    let (new_page_num, new_ph) = PagedFile::alloc_new_page(&*self.file, self.page_size, Some(self.pages[self.curr_page_idx]))?;
                    self.file.write_all_at(&new_ph.value.to_le_bytes(), new_page_num * self.page_size)?;
                    self.update_last_page_header(Some(new_page_num))?;
                    self.pages.push(new_page_num);
                    self.curr_page_idx += 1;
                    self.pos_in_page = 0;
                }
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
  
    fn write_all(&mut self, mut buf: &[u8]) -> std::io::Result<()> {
        self.write(&mut buf)?;
        Ok(()) // our implementation always writes all bytes
    }
}

impl Seek for PFileBlockCachedHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(n) => {
                let n_pages = (n / self.page_capacity()) as usize;
                if n_pages >= self.pages.len() {
                    // tried to seek past end
                    self.curr_page_idx = self.pages.len() - 1;
                    self.pos_in_page = self.last_page_len;
                } else if n_pages == self.pages.len() - 1 {
                    self.curr_page_idx = n_pages;
                    self.pos_in_page = (n % self.page_capacity()).min(self.last_page_len);
                } else {
                    self.curr_page_idx = n_pages;
                    self.pos_in_page = n % self.page_capacity();
                }
                Ok(self.curr_page_idx as u64 * self.page_capacity() + self.pos_in_page)
            },
            std::io::SeekFrom::Current(off) => {
                let curr_pos = self.curr_page_idx as u64 * self.page_capacity() + self.pos_in_page;
                if (curr_pos as i64 + off) < 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Seek before start of block"));
                }
                self.seek(std::io::SeekFrom::Start((curr_pos as i64 + off) as u64))
            },
            std::io::SeekFrom::End(off) => {
                let end = (self.pages.len() - 1) as u64 * self.page_capacity() + self.last_page_len;
                if (end as i64 + off) < 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Seek before start of block"));
                }
                self.seek(std::io::SeekFrom::Start((end as i64 + off) as u64))
            },
        }
    }

    fn stream_position(&mut self) -> std::io::Result<u64> {
        Ok(self.curr_page_idx as u64 * self.page_capacity() + self.pos_in_page)
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::SeekFrom};

    const PAGE_SIZE: usize = 2048;
    const PAGE_CAPACITY: usize = PAGE_SIZE - 8;
    fn new_paged_file() -> PagedFile {
        PagedFile::new(std::cell::RefCell::new(Vec::new()), PAGE_SIZE as u64)
    }
 
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 256) as u8).collect()
    }
 
    // ---------------------------------------------------------------
    // Basic read/write correctness
    // ---------------------------------------------------------------
 
    #[test]
    fn new_block_is_empty() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let mut buf = [0u8; 16];
        let n = block.read(&mut buf).unwrap();
        assert_eq!(n, 0, "freshly created block should have no data to read");
    }
 
    #[test]
    fn write_then_read_back_small() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(b"hello world").unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 11];
        block.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
    }
 
    #[test]
    fn read_past_end_returns_short_read_not_error() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(b"short").unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = [0u8; 100];
        let n = block.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"short");
    }
 
    #[test]
    fn write_spans_multiple_pages() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let data = pattern(PAGE_CAPACITY * 3 + 123); // spans 4 pages
        block.write_all(&data).unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut readback = vec![0u8; data.len()];
        block.read_exact(&mut readback).unwrap();
        assert_eq!(readback, data);
    }
 
    #[test]
    fn write_exact_page_capacity_then_read_back() {
        // Exercises the boundary where a page is filled to exactly its
        // capacity without a following page ever being allocated.
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let data = pattern(PAGE_CAPACITY);
        block.write_all(&data).unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut readback = vec![0u8; PAGE_CAPACITY];
        block.read_exact(&mut readback).unwrap();
        assert_eq!(readback, data);
 
        // Reading further should yield a clean EOF (0 bytes), not an error.
        let mut extra = [0u8; 8];
        let n = block.read(&mut extra).unwrap();
        assert_eq!(n, 0);
    }
 
    #[test]
    fn many_small_writes_accumulate_correctly() {
        // Writes data via many small `write()` calls (rather than one
        // `write_all`) to exercise position tracking across calls, including
        // calls that straddle a page boundary.
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let data = pattern(PAGE_CAPACITY + 500);
        for chunk in data.chunks(37) {
            block.write_all(chunk).unwrap();
        }
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut readback = vec![0u8; data.len()];
        block.read_exact(&mut readback).unwrap();
        assert_eq!(readback, data);
    }
 
    #[test]
    fn overwrite_within_existing_bounds_does_not_truncate() {
        // Writing over already-written bytes (without extending the block)
        // must not shrink the recorded page length.
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(100)).unwrap();
        block.seek(SeekFrom::Start(10)).unwrap();
        block.write_all(&[0xAAu8; 5]).unwrap();
 
        block.seek(SeekFrom::Start(0)).unwrap();
        let mut readback = vec![0u8; 100];
        block.read_exact(&mut readback).unwrap();
 
        let mut expected = pattern(100);
        expected[10..15].copy_from_slice(&[0xAAu8; 5]);
        assert_eq!(readback, expected);
    }
 
    // ---------------------------------------------------------------
    // Seeking
    // ---------------------------------------------------------------
 
    #[test]
    fn seek_current_forward_and_backward_within_page() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(200)).unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
 
        block.seek(SeekFrom::Current(50)).unwrap();
        let mut buf = [0u8; 4];
        block.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [50, 51, 52, 53]);
 
        block.seek(SeekFrom::Current(-10)).unwrap();
        let mut buf2 = [0u8; 4];
        block.read_exact(&mut buf2).unwrap();
        assert_eq!(buf2, [44, 45, 46, 47]);
    }
 
    #[test]
    fn seek_forward_across_page_boundary() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let data = pattern(PAGE_CAPACITY + 100);
        block.write_all(&data).unwrap();
        block.seek(SeekFrom::Start((PAGE_CAPACITY - 5) as u64)).unwrap();
        let mut buf = [0u8; 10];
        block.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[PAGE_CAPACITY - 5..PAGE_CAPACITY + 5]);
    }
 
    #[test]
    fn seek_backward_across_page_boundary() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        let data = pattern(PAGE_CAPACITY + 100);
        block.write_all(&data).unwrap();
        // currently positioned at the end (PAGE_CAPACITY + 100)
        block.seek(SeekFrom::Current(-105)).unwrap();
        let mut buf = [0u8; 10];
        block.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[PAGE_CAPACITY - 5..PAGE_CAPACITY + 5]);
    }
 
    #[test]
    fn seek_before_start_of_block_errors() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(50)).unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let err = block.seek(SeekFrom::Current(-1)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
 
    #[test]
    fn seek_current_zero_is_a_no_op() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(50)).unwrap();
        block.seek(SeekFrom::Start(20)).unwrap();
        let pos = block.seek(SeekFrom::Current(0)).unwrap();
        assert_eq!(pos, 20);
    }

    #[test]
    fn seek_end_single_partial_page() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(100)).unwrap();
        block.seek(SeekFrom::Start(0)).unwrap();
        let pos = block.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, 100);
    }
 
    #[test]
    fn seek_end_across_multiple_pages() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(3000)).unwrap(); // page0 full (2040) + page1 partial (960)
        block.seek(SeekFrom::Start(0)).unwrap();
        let pos = block.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, 3000);
    }
 
    #[test]
    fn seek_end_when_last_page_exactly_full() {
        let pf = new_paged_file();
        let (_, mut block) = pf.create_new_block().unwrap();
        block.write_all(&pattern(PAGE_CAPACITY)).unwrap(); // fills page 0 exactly, no next page allocated
        let pos = block.seek(SeekFrom::End(0)).unwrap();
        assert_eq!(pos, PAGE_CAPACITY as u64);
    }
 
    // ---------------------------------------------------------------
    // Multiple blocks / reopening
    // ---------------------------------------------------------------
 
    #[test]
    fn multiple_blocks_do_not_interfere() {
        let pf = new_paged_file();
        let (_, mut block_a) = pf.create_new_block().unwrap();
        let id_a = block_a_page(&block_a);
        block_a.write_all(b"AAAA-block").unwrap();
 
        let (_, mut block_b) = pf.create_new_block().unwrap();
        let id_b = block_a_page(&block_b);
        block_b.write_all(b"BBBB-block").unwrap();
 
        let mut reopened_a = pf.open_existing_block(id_a).unwrap();
        let mut buf_a = [0u8; 10];
        reopened_a.read_exact(&mut buf_a).unwrap();
        assert_eq!(&buf_a, b"AAAA-block");
 
        let mut reopened_b = pf.open_existing_block(id_b).unwrap();
        let mut buf_b = [0u8; 10];
        reopened_b.read_exact(&mut buf_b).unwrap();
        assert_eq!(&buf_b, b"BBBB-block");
    }
 
    #[test]
    fn reopened_block_reads_data_written_before_it_was_closed() {
        let pf = new_paged_file();
        let id = {
            let (_, mut block) = pf.create_new_block().unwrap();
            let id = block_a_page(&block); // capture the *starting* page id before writing moves us onto later pages
            let data = pattern(PAGE_CAPACITY + 50);
            block.write_all(&data).unwrap();
            id
        };
        let mut reopened = pf.open_existing_block(id).unwrap();
        let mut readback = vec![0u8; PAGE_CAPACITY + 50];
        reopened.read_exact(&mut readback).unwrap();
        assert_eq!(readback, pattern(PAGE_CAPACITY + 50));
    }
 
    #[test]
    fn open_existing_block_on_continuation_page_errors() {
        let pf = new_paged_file();
        let second_page_id = {
            let (_, mut block) = pf.create_new_block().unwrap();
            // force allocation of a second, continuation page
            block.write_all(&pattern(PAGE_CAPACITY + 10)).unwrap();
            block.current_page // now sitting on the continuation page
        };
        match pf.open_existing_block(second_page_id) {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            Ok(_) => panic!("expected an error when opening a continuation page as a block"),
        }
    }
 
    // Small helper exposing the private `current_page` field for test setup;
    // kept separate so call sites above read clearly.
    fn block_a_page(block: &PFileBlockHandle) -> u64 {
        block.current_page
    }
}