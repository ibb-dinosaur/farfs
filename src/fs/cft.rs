use std::io::{Read, Seek, Write, const_error};

use crate::{fs::FileRef, paging::PFileBlockCachedHandle};
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

    /// must be called right after read_file_record so the reader is in the right position to write the lock flag
    pub fn lock_file(&mut self, file_record: &mut FileRecord) -> std::io::Result<()> {
        debug_assert!(self.0.stream_position()? == file_record.uid * 128 + 128);
        // byte-based OS-level lock here ?
        if file_record.flags.is_excl_lock() {
            return Err(const_error!(std::io::ErrorKind::ResourceBusy, "File is currently locked"));
        }
        file_record.flags.set_is_excl_lock(true);
        self.0.seek(std::io::SeekFrom::Current(-117))?;
        self.0.write_all(&[1])?;
        self.0.seek(std::io::SeekFrom::Current(116))?;
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
    
    pub(crate) is_excl_lock, set_is_excl_lock : 24;
}

/// Dyndata is a storage space for short data optimization, long file names, and any short bytes that are not worth their own block.
/// It is a list of "slices", each prefixed with a varint length.
/// The length is encoded as unsigned LEB128. The lowest bit of the length indicates
/// whether the slice is in use (1) or free (0).
pub(crate) struct Dyndata {
    handle: PFileBlockCachedHandle,
    recently_freed: Vec<(u32, u32)>, // (offset, length) of recently freed slices, for reuse
}

const MAX_RECENTLY_FREED: usize = 16;

impl Dyndata {
    pub(crate) fn new(handle: PFileBlockCachedHandle) -> Self {
        Dyndata { handle, recently_freed: Vec::with_capacity(MAX_RECENTLY_FREED) }
    }

    pub(crate) fn store_slice(&mut self, data: &[u8]) -> std::io::Result<u64> {
        let pos;
        if let Some((index, (offset, _))) = 
            self.recently_freed.iter().enumerate()
                .filter(|(_, (_, len))| *len as usize >= data.len())
                .min_by_key(|(_, (_, len))| *len) {
            pos = *offset as u64;
            self.recently_freed.remove(index);
        } else {
            pos = self.handle.seek(std::io::SeekFrom::End(0))?;
        }
        self._write_slice(data)?;
        Ok(pos)
    }

    fn _write_slice(&mut self, data: &[u8]) -> std::io::Result<()> {
        #[cfg(debug_assertions)]
        if data.len() > 65535 {
            eprintln!("warn: a large slice stored in dyndata ({} bytes)", data.len());
        }
        varint_rs::VarintWriter::write_usize_varint(&mut self.handle, (data.len() << 1) | 1)?;
        self.handle.write_all(data)
    }

    pub(crate) fn get_slice(&mut self, offset: u64) -> std::io::Result<Vec<u8>> {
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        let n = varint_rs::VarintReader::read_usize_varint(&mut self.handle)?;
        let (len, in_use) = (n >> 1, n & 1 == 1);
        if !in_use {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Slice is not in use"));
        }
        let mut data = vec![0u8; len];
        self.handle.read_exact(&mut data)?;
        Ok(data)
    }

    pub(crate) fn try_update_slice(&mut self, offset: u64, new_data: &[u8]) -> std::io::Result<u64> {
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        let n = varint_rs::VarintReader::read_usize_varint(&mut self.handle)?;
        let m = self.handle.stream_position()?;
        let (len, in_use) = (n >> 1, n & 1 == 1);
        if !in_use {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Slice is not in use"));
        }
        if new_data.len() > len || new_data.is_empty() {
            // free current slice
            self.handle.seek(std::io::SeekFrom::Start(offset))?; // go back
            varint_rs::VarintWriter::write_usize_varint(&mut self.handle, 
                (len << 1) | 0)?; // 0 = free
            if self.recently_freed.len() < MAX_RECENTLY_FREED {
                self.recently_freed.push((offset as u32, len as u32));
            }
            if new_data.is_empty() { Ok(0) }
            else { self.store_slice(new_data) } // allocate new slice
        } else {
            // overwrite in place
            self.handle.seek(std::io::SeekFrom::Start(offset))?; // go back
            varint_rs::VarintWriter::write_usize_varint(&mut self.handle, (new_data.len() << 1) | 1)?;
            self.handle.write_all(new_data)?;
            let leftover = m + len as u64 - self.handle.stream_position()?;
            if leftover > 0 {
                // conservatively assume the varint will take up 4 bytes.
                varint_rs::VarintWriter::write_usize_varint(&mut self.handle, 
                    (leftover as usize - 4) << 1 | 0)?; // 0 = free
                while self.handle.stream_position()? < m + len as u64 {
                    self.handle.write_all(&[0])?;
                }
            }
            Ok(offset) // no change in position
        }
    }
}