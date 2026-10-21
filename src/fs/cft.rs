use std::io::{Read, Seek, Write, const_error};

use crate::{fs::{FileRef}, paging::PFileBlockCachedHandle};
use bitfield::bitfield;
use bytemuck::{AnyBitPattern, NoUninit, Pod, Zeroable};

/// The Central File Table (CFT) holds all the FileRecords
pub(crate) struct Cft(PFileBlockCachedHandle);

impl Cft {
    pub fn new(handle: PFileBlockCachedHandle) -> Self {
        Cft(handle)
    }
    
    pub fn read_file_record(&mut self, fr: FileRef) -> std::io::Result<Option<FileRecord>> {
        self.0.seek(std::io::SeekFrom::Start(fr.uid * 128))?;
        let mut file_record = FileRecord::zeroed();
        let n = self.0.read(bytemuck::bytes_of_mut(&mut file_record))?;
        if n < 128 { return Ok(None); }
        Ok(Some(file_record))
    }

    pub fn write_file_record(&mut self, file_record: &FileRecord) -> std::io::Result<()> {
        self.0.seek(std::io::SeekFrom::Start(file_record.uid * 128))?;
        self.0.write_all(bytemuck::bytes_of(file_record))
    }

    pub fn alloc_new_file_record(&mut self) -> std::io::Result<FileRecord> {
        // TODO: reuse deleted file records
        let new_file_uid = self.0.seek(std::io::SeekFrom::End(0))? / 128;
        let mut new_file_header = FileRecord::zeroed();
        new_file_header.uid = new_file_uid;
        new_file_header.flags.set_is_not_empty(true);
        self.0.write_all(bytemuck::bytes_of(&new_file_header))?;
        Ok(new_file_header)
    }

    pub fn delete_file_record(&mut self, file_record: FileRef) -> std::io::Result<()> {
        // Clear the file record
        self.0.seek(std::io::SeekFrom::Start(file_record.uid * 128))?;
        self.0.write_all(&[0; 128])
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[repr(C)]
#[derive(Clone, Debug, Copy, AnyBitPattern, NoUninit)]
pub(crate) struct FileRecord {
    pub(crate) uid: u64,
    pub(crate) flags: FileFlagsRaw,
    pub(crate) _reserved1: [u8; 4],
    pub(crate) parent_uid: u64,
    pub(crate) size: u64,
    pub(crate) start_block: u64,
    pub(crate) create_time: u64,
    pub(crate) modify_time: u64,
    pub(crate) access_time: u64,
    /// if flags.longname is set:
    /// - the full name is stored in dyndata
    /// - last 8 bytes of this are the position
    /// - first 16 bytes are the first 16 bytes of the name for quick comparison
    pub(crate) shortname: [u8; 24],
    /// user-defined attributes, stored in dyndata. 0 = none
    pub(crate) user_attrs_pos: u64,
    pub(crate) _reserved2: [u8; 32],
}


bitfield! {
    #[derive(Clone, Copy, Zeroable, Pod)]
    #[repr(transparent)]
    pub(crate) struct FileFlagsRaw(u32);
    impl Debug;
    pub(crate) is_not_empty, set_is_not_empty : 0;
    pub(crate) is_dir, set_dir : 1;
    pub(crate) is_readonly, set_readonly : 2;

    // Short Data Optimization
    // For both directories and files: if this is set and start_block=0, the file/dir is empty
    // For directories: on first added file, allocate 40 bytes (5 files) in dyndata and use that as directory listing. 
    //      If more than 5 files are added, allocate a block and move the listing there.
    pub(crate) is_sdo, set_sdo : 11;
    pub(crate) has_longname, set_has_longname : 12;
    }

/// Dyndata is a storage space for short data optimization, long file names, and any short bytes that are not worth their own block.
/// It is a list of "slices", each prefixed with a varint length.
/// The length is encoded as unsigned LEB128. The lowest bit of the length indicates
/// whether the slice is in use (1) or free (0).
pub(crate) struct Dyndata {
    handle: PFileBlockCachedHandle,
    // (offset, total size in bytes including header) of recently freed slots, for reuse.
    recently_freed: Vec<(u64, usize)>,
}

const MAX_RECENTLY_FREED: usize = 16;
const LARGE_SLICE_WARN_THRESHOLD: usize = 65535;
const SLICE_SIZE_HARD_LIMIT: usize = 1 << 27;

/// Number of bytes `n` takes as an unsigned LEB128 varint.
fn varint_len(n: usize) -> usize {
    let bits = (usize::BITS - n.leading_zeros()) as usize;
    bits.div_ceil(7).max(1)
}

/// The header value for a slice of `len` bytes: `len << 1 | in_use`
fn encode_header(len: usize, in_use: bool) -> usize {
    (len << 1) | (in_use as usize)
}

/// Total bytes a *used* slice with `len` data bytes occupies (header + data).
fn slot_size(len: usize) -> usize {
    varint_len(encode_header(len, true)) + len
}

impl Dyndata {
    pub(crate) fn new(handle: PFileBlockCachedHandle) -> std::io::Result<Self> {
        let this = Dyndata { handle, recently_freed: Vec::with_capacity(MAX_RECENTLY_FREED) };
        Ok(this)
    }

    /// Records a free region for reuse, if it's useful and there is room.
    fn track_free(&mut self, offset: u64, total: usize) {
        if total >= 2 && self.recently_freed.len() < MAX_RECENTLY_FREED {
            self.recently_freed.push((offset, total));
        }
    }

    pub(crate) fn store_slice(&mut self, data: &[u8]) -> std::io::Result<u64> {
        if data.len() > SLICE_SIZE_HARD_LIMIT {
            return Err(const_error!(std::io::ErrorKind::InvalidInput, "Slice too large"));
        }
        let need = slot_size(data.len());
        // Best fit: smallest free slot that is big enough.
        let reuse = self
            .recently_freed
            .iter()
            .enumerate()
            .filter(|(_, (_, total))| *total >= need)
            .min_by_key(|(_, (_, total))| *total)
            .map(|(index, (offset, total))| (index, *offset, *total));

        match reuse {
            Some((index, offset, total)) => {
                self.handle.seek(std::io::SeekFrom::Start(offset))?;
                self._write_slice(data)?;
                self.recently_freed.remove(index);
                // Turn whatever is left of the old slot into valid free space.
                self._write_free_region(offset + need as u64, total - need)?;
                Ok(offset)
            }
            None => {
                let pos = self.handle.seek(std::io::SeekFrom::End(0))?;
                self._write_slice(data)?;
                Ok(pos)
            }
        }
    }

    /// Writes header + data at the current position.
    fn _write_slice(&mut self, data: &[u8]) -> std::io::Result<()> {
        #[cfg(debug_assertions)]
        if data.len() > LARGE_SLICE_WARN_THRESHOLD {
            eprintln!("warn: a large slice stored in dyndata ({} bytes)", data.len());
        }
        let header = encode_header(data.len(), true);
        varint_rs::VarintWriter::write_usize_varint(&mut self.handle, header)?;
        self.handle.write_all(data)
    }

    /// Marks `total` bytes starting at `offset` as free, so the slice list stays valid.
    ///
    /// Writes one free slice that covers as much of the region as possible, followed by
    /// zero bytes (free, zero-length slices) for any remainder that can't be encoded
    /// exactly. Records the region for reuse.
    fn _write_free_region(&mut self, offset: u64, total: usize) -> std::io::Result<()> {
        const PAD: [u8; 4] = [0u8; 4];
        if total == 0 {
            return Ok(());
        }
        if total < 3 {
            self.handle.seek(std::io::SeekFrom::Start(offset))?;
            return self.handle.write_all(&PAD[..total]);
        }
        let expected_header_len = varint_len(encode_header(total, false));
        let payload = total - expected_header_len;
        let pad = total - payload - varint_len(encode_header(payload, false));
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        varint_rs::VarintWriter::write_usize_varint(&mut self.handle, encode_header(payload, false))?;
        if pad > 0 {
            self.handle.seek(std::io::SeekFrom::Current(payload as i64))?;
            self.handle.write_all(&PAD[..pad])?;
        }
        self.track_free(offset, total);
        Ok(())
    }

    pub(crate) fn get_slice(&mut self, offset: u64) -> std::io::Result<Vec<u8>> {
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        let n = varint_rs::VarintReader::read_usize_varint(&mut self.handle)?;
        let (len, in_use) = (n >> 1, n & 1 == 1);
        if !in_use {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Slice is not in use"));
        }
        if len > SLICE_SIZE_HARD_LIMIT {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Slice too large"));
        }
        let data_start = self.handle.stream_position()?;
        let end = self.handle.seek(std::io::SeekFrom::End(0))?;
        if len as u64 > end.saturating_sub(data_start) {
            return Err(const_error!(
                std::io::ErrorKind::InvalidData,
                "Slice length exceeds file size"
            ));
        }
        self.handle.seek(std::io::SeekFrom::Start(data_start))?;
        let mut data = vec![0u8; len];
        self.handle.read_exact(&mut data)?;
        Ok(data)
    }

    /// Replaces the slice at `offset` with `new_data` and returns its (possibly new)
    /// offset. Returns `Ok(0)` if `new_data` is empty (the slice is freed).
    pub(crate) fn try_update_slice(&mut self, offset: u64, new_data: &[u8]) -> std::io::Result<u64> {
        if new_data.len() > SLICE_SIZE_HARD_LIMIT {
            return Err(const_error!(std::io::ErrorKind::InvalidInput, "Slice too large"));
        }
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        let n = varint_rs::VarintReader::read_usize_varint(&mut self.handle)?;
        let header_len = (self.handle.stream_position()? - offset) as usize;
        let (len, in_use) = (n >> 1, n & 1 == 1);
        if !in_use {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Slice is not in use"));
        }
        let old_total = header_len + len;

        if new_data.is_empty() {
            self._write_free_region(offset, old_total)?;
            return Ok(0);
        }

        if new_data.len() > len {
            // Allocate the new slice *first*: if that fails, the old data is untouched.
            let new_offset = self.store_slice(new_data)?;
            self._write_free_region(offset, old_total)?;
            return Ok(new_offset);
        }

        // Overwrite in place; the shrunken tail becomes a valid free region.
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        self._write_slice(new_data)?;
        let used = slot_size(new_data.len()); // <= old_total, since new len <= len
        self._write_free_region(offset + used as u64, old_total - used)?;
        Ok(offset) // no change in position
    }

    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.handle.flush()
    }
}