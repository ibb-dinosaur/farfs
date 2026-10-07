use std::{borrow::Cow, io::{Seek, Write, const_error}};

use crate::{fs::*, paging::{PFileBlockHandle, PagedFile}, util::null_terminated_string};

#[derive(Debug)]
pub struct FileInfo {
    pub name: Vec<u8>,
    pub self_ref: FileRef,
    pub is_directory: bool,
    pub is_readonly: bool,
    pub parent: FileRef,
    pub size: u64,
    pub create_time: u64,
    pub modify_time: Option<u64>,
    pub access_time: Option<u64>,
    /// User-defined attributes
    pub user_attrs: Option<Vec<u8>>,
}

impl FileRecord { 
    pub(crate) fn get_filename<'a>(&'a self, s: &Shared) -> std::io::Result<Cow<'a, [u8]>> {
        if self.flags.has_longname() {
            let longname_location = u64::from_le_bytes(self.shortname[16..24].try_into().unwrap());
            let fullname = s.dyndata.lock().unwrap().get_slice(longname_location)?;
            debug_assert!(fullname[..16] == self.shortname[..16]);
            Ok(Cow::Owned(fullname))
        } else {
            Ok(Cow::Borrowed(null_terminated_string(&self.shortname)))
        }
    }
}

impl FileInfo {
    pub(crate) fn from_record(value: FileRecord, s: &Shared) -> std::io::Result<Self> {
        let user_attrs =
            if value.user_attrs_pos != 0 {
                Some(s.dyndata.lock().unwrap().get_slice(value.user_attrs_pos)?)
            } else { None };
        Ok(FileInfo {
            self_ref: FileRef { uid: value.uid },
            name: value.get_filename(s)?.into_owned(),
            is_directory: value.flags.is_dir(),
            is_readonly: value.flags.is_readonly(),
            parent: FileRef { uid: value.parent_uid },
            size: value.size,
            create_time: value.create_time,
            modify_time: if !value.flags.is_dir() { Some(value.modify_time) } else { None },
            access_time: if !value.flags.is_dir() { Some(value.access_time) } else { None },
            user_attrs,
        })
    }
}

/// An owning reference to a properly opened file.
pub struct FileHandle<'dr> {
    header: Box<FileRecord>,
    data: FileDataReader,
    s: &'dr Shared,
    modified: bool,
}

impl<'dr> FileHandle<'dr> {
    /// Create a new FileHandle.
    /// This assumes the file record has been checked, locked, and the file is valid to be opened.
    pub(crate) fn new(record: FileRecord, s: &'dr Shared) -> std::io::Result<Self> {
        let data = 
            if record.flags.is_sdo() || record.start_block == 0 {
                FileDataReader::SdoLazyNone(s.file.clone())
            } else {
                FileDataReader::LongFile(s.file.open_existing_block(record.start_block)?)
            };
        Ok(Self { header: Box::new(record), data, s, modified: false })
    }

    /// Change the name of the file in the file record. Doesn't write to the CFT.
    pub(crate) fn change_name(record: &mut FileRecord, dyndata: &mut Dyndata, name: &[u8]) -> std::io::Result<()> {
        if name.len() > 24 {
            let longname_loc = if record.flags.has_longname() {
                let old_longname_location = u64::from_le_bytes(record.shortname[16..24].try_into().unwrap());
                dyndata.try_update_slice(old_longname_location, name)?
            } else {
                dyndata.store_slice(name)?
            };
            record.shortname[0..16].copy_from_slice(&name[0..16]);
            record.shortname[16..24].copy_from_slice(&longname_loc.to_le_bytes());
        } else {
            if record.flags.has_longname() {
                let old_longname_location = u64::from_le_bytes(record.shortname[16..24].try_into().unwrap());
                dyndata.try_update_slice(old_longname_location, &[])?; // free old longname
            }
            record.shortname = [0; 24];
            record.shortname[..name.len()].copy_from_slice(name);
        }
        record.flags.set_has_longname(name.len() > 24);
        Ok(())
    }
}

impl FileHandle<'_> {
    fn write_file_record_to_disk(&self) -> std::io::Result<()> {
        let mut cft = self.s.cft.lock().unwrap();
        cft.write_file_record(&*self.header)
    }

    pub fn file_size(&self) -> u64 {
        self.header.size
    }

    pub fn allocation_size(&self) -> u64 {
        self.header.size.div_ceil(self.s.file.page_capacity()) * self.s.file.page_size()
    }

    pub fn parent_dir(&self) -> FileRef {
        FileRef { uid: self.header.parent_uid }
    }

    pub fn name(&self) -> Cow<'_, [u8]> {
        self.header.get_filename(self.s).unwrap()
    }

    pub fn set_name(&mut self, name: &[u8]) -> std::io::Result<()> {
        self.modified = true;
        self.s.path_cache.drop(self.header.parent_uid, self.name().as_ref());
        Self::change_name(&mut self.header, &mut *self.s.dyndata.lock().unwrap(), name)
    }

    pub fn is_readonly(&self) -> bool {
        self.header.flags.is_readonly()
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.header.flags.set_readonly(readonly);
        self.modified = true;
    }

    pub fn get_ref(&self) -> FileRef {
        FileRef { uid: self.header.uid }
    }

    pub fn create_time(&self) -> u64 {
        self.header.create_time
    }

    pub fn access_time(&self) -> u64 {
        self.header.access_time
    }

    pub fn set_access_time(&mut self, time: u64) {
        self.header.access_time = time;
    }

    pub fn modify_time(&self) -> u64 {
        self.header.modify_time
    }

    pub fn set_modify_time(&mut self, time: u64) {
        if !self.header.flags.is_readonly() {
            self.header.modify_time = time;
        }
    }

    pub fn full_fileinfo(&self) -> std::io::Result<FileInfo> {
        FileInfo::from_record(*self.header, self.s)
    }

    pub fn user_attrs(&self) -> std::io::Result<Option<Vec<u8>>> {
        if self.header.user_attrs_pos != 0 {
            Ok(Some(self.s.dyndata.lock().unwrap().get_slice(self.header.user_attrs_pos)?))
        } else { Ok(None) }
    }

    pub fn set_user_attrs(&mut self, attrs: &[u8]) -> std::io::Result<()> {
        if self.header.user_attrs_pos != 0 {
            self.header.user_attrs_pos = self.s.dyndata.lock().unwrap().try_update_slice(
                self.header.user_attrs_pos, attrs)?;
        } else {
            self.header.user_attrs_pos = self.s.dyndata.lock().unwrap().store_slice(attrs)?;
        }
        Ok(())
    }

    pub fn set_len(&mut self, size: u64) -> std::io::Result<()> {
        if self.header.flags.is_readonly() {
            return Err(const_error!(std::io::ErrorKind::PermissionDenied, "File is read-only"));
        }
        self.write(&[])?; // ensure the file is allocated if it was SDO
        let h = match &mut self.data {
            FileDataReader::LongFile(h) => h,
            _ => unreachable!()
        };
        if size < self.header.size {
            // truncate
            h.seek(std::io::SeekFrom::Start(size))?;
            h.shrink()?;
            self.header.size = size;
            self.modified = true;
            Ok(())
        } else if size > self.header.size {
            // extend
            let mut current_size = h.seek(std::io::SeekFrom::End(0))?;
            static ZEROS: [u8; 4096] = [0u8; 4096];
            while current_size < size {
                let to_write = ZEROS.len().min((size - current_size) as usize);
                h.write_all(&ZEROS[..to_write])?;
                current_size += to_write as u64;
            }
            self.header.size = size;
            self.modified = true;
            Ok(())
        } else {
            Ok(())
        }
    }   

    pub fn close(self) -> std::io::Result<()> {
        std::mem::drop(self);
        Ok(())
    }
}

impl std::io::Read for FileHandle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.read(buf),
            FileDataReader::SdoLazyNone(_) => Ok(0), // no data
        }
    }
}

impl std::io::Write for FileHandle<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.header.flags.is_readonly() {
            return Err(const_error!(std::io::ErrorKind::PermissionDenied, "File is read-only"));
        }
        self.modified = true;
        match &mut self.data {
            FileDataReader::LongFile(h) => {
                let res = h.write(buf);
                if h.stream_position()? > self.header.size {
                    self.header.size = h.stream_position()?;
                }
                res
            },
            FileDataReader::SdoLazyNone(paged_file) => {
                // user wants to write -> we need to allocate a block
                let (block_n, handle) = paged_file.create_new_block()?;
                self.header.start_block = block_n;
                self.header.flags.set_sdo(false);
                self.flush()?;
                self.data = FileDataReader::LongFile(handle);
                self.write(buf)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.flush()?,
            FileDataReader::SdoLazyNone(_) => {},
        };
        self.header.access_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        self.write_file_record_to_disk()
    }
}

impl std::io::Seek for FileHandle<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.seek(pos),
            FileDataReader::SdoLazyNone(_) => match pos {
                std::io::SeekFrom::Current(n) | std::io::SeekFrom::End(n) if n < 0 =>
                    Err(const_error!(std::io::ErrorKind::InvalidInput, "Cannot seek before start of file")),
                _ => Ok(0)
            },
        }
    }
}

impl Drop for FileHandle<'_> {
    fn drop(&mut self) {
        // make sure all data changes are written to disk
        match &mut self.data {
            FileDataReader::LongFile(h) => {
                let _ = h.flush();
            },
            FileDataReader::SdoLazyNone(_) => {},
        }
        // update file metadata as neccessary
        self.header.access_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        if self.modified {
            self.header.modify_time = self.header.access_time;
        }
        self.header.flags.set_is_excl_lock(false);
        let _ = self.write_file_record_to_disk();
    }
}

enum FileDataReader {
    SdoLazyNone(PagedFile), // SDO: no data yet, lazy block allocation on first write
    LongFile(PFileBlockHandle),
}