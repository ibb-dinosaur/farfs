use std::{borrow::Cow, io::{Read, Seek, Write, const_error}, sync::Mutex};

use bitfield::bitfield;
use bytemuck::{AnyBitPattern, NoUninit, Pod, Zeroable};

use crate::{paging::{PFileBlockCachedHandle, PFileBlockHandle, PagedFile}, util::FileLike};

#[repr(C)]
#[derive(Clone, Copy)]
struct DriveHeader {
    magic: [u8; 7], // "FARCDRV"
    version: u8, // 1
    drive_name: [u8; 24], // nt utf-8
    drive_owner: [u8; 24], // nt utf-8
    page_size: u8, // log2 of page size in bytes

    _reserved: [u8; 248-57],
}
unsafe impl Zeroable for DriveHeader {}
unsafe impl AnyBitPattern for DriveHeader {}
unsafe impl NoUninit for DriveHeader {}

const DRIVE_HEADER_BLOCK: u64 = 0;
const CENTRAL_FILE_DIRECTORY_BLOCK: u64 = 1;
const DYNDATA_BLOCK: u64 = 2;

struct Shared {
    file: PagedFile,
    cfd: Mutex<PFileBlockCachedHandle>,
    dyndata: Mutex<Dyndata>,
}

pub struct Drive {
    s: Box<Shared>,
    header: Box<DriveHeader>,
}

pub struct DriveConf<'s> {
    /// Must be a power of two, and at least 256. Defaults to 2048.
    pub page_size: u64,
    /// At most 24 bytes.
    pub drive_name: &'s [u8],
    /// At most 24 bytes.
    pub drive_owner: &'s [u8],
}

impl Default for DriveConf<'_> {
    fn default() -> Self {
        DriveConf {
            page_size: 2048,
            drive_name: b"",
            drive_owner: b"",
        }
    }
}

impl Drive {
    pub fn new_create(backing: impl FileLike + 'static, conf: DriveConf<'_>) -> std::io::Result<Self> {
        assert!(conf.page_size.is_power_of_two() && conf.page_size >= 256, "page_size must be a power of two and at least 256");
        assert!(conf.drive_name.len() <= 24, "drive_name must be at most 24 bytes");
        assert!(conf.drive_owner.len() <= 24, "drive_owner must be at most 24 bytes");

        let source = PagedFile::new(backing, conf.page_size);
        let (n, mut drive_header) = source.create_new_block()?;
        assert!(n == DRIVE_HEADER_BLOCK);
        let (n, mut cfd) = source.create_new_block_cached()?;
        assert!(n == CENTRAL_FILE_DIRECTORY_BLOCK);
        let (n, dyndata) = source.create_new_block_cached()?;
        assert!(n == DYNDATA_BLOCK);
        let mut dyndata = Dyndata::new(dyndata);
        dyndata.store_slice(&[0xFF])?; // this is to make sure 0 is not a valid position for dyndata
        let mut dr_header = DriveHeader {
            magic: *b"FARCDRV",
            version: 1,
            drive_name: [0; 24],
            drive_owner: [0; 24],
            page_size: conf.page_size.trailing_zeros() as u8,
            _reserved: [0; _],
        };
        dr_header.drive_name[..conf.drive_name.len()].copy_from_slice(conf.drive_name);
        dr_header.drive_owner[..conf.drive_owner.len()].copy_from_slice(conf.drive_owner);
        drive_header.write_all(bytemuck::bytes_of(&dr_header))?;
        // create root dir
        let mut root_file_header = FileEntry::zeroed();
        root_file_header.flags.set_is_not_empty(true);
        root_file_header.flags.set_dir(true);
        root_file_header.start_block = source.create_new_block()?.0;
        root_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        cfd.write_all(bytemuck::bytes_of(&root_file_header))?;

        Ok(Drive { s: Box::new(Shared { file: source, cfd: Mutex::new(cfd), dyndata: Mutex::new(dyndata) }), header: Box::new(dr_header) })
    }

    pub fn new_existing(backing: impl FileLike + 'static) -> std::io::Result<Self> {
        let mut buf = [0u8; 256]; // first 8 bytes - paging info, then - drive header
        backing.read_exact_at(&mut buf, 0)?;
        let drive_header: &DriveHeader = bytemuck::from_bytes(&buf[8..256]);
        if drive_header.magic != *b"FARCDRV" || drive_header.version != 1 {
            return Err(const_error!(std::io::ErrorKind::InvalidData, "Invalid drive header"));
        }
        let page_size = 1u64 << drive_header.page_size;
        let source = PagedFile::new(backing, page_size);
        let cfd = source.open_existing_block_cached(CENTRAL_FILE_DIRECTORY_BLOCK)?;
        let dyndata = source.open_existing_block_cached(DYNDATA_BLOCK)?;
        Ok(Drive { s: Box::new(Shared { file: source, cfd: Mutex::new(cfd), dyndata: Mutex::new(Dyndata::new(dyndata)) }), header: Box::new(*drive_header) })
    }

    /// Open a file for reading and writing.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn open<'a>(&'a self, file: impl ResolveToExisting) -> std::io::Result<FileHandle<'a>> {
        let file = file.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let mut cfd = self.s.cfd.lock().unwrap();        
        let mut file_entry = 
            read_file_entry(&mut cfd, file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        
        assert!(file_entry.flags.is_not_empty());
        assert!(file_entry.uid == file.uid);
        if file_entry.flags.is_dir() {
            return Err(const_error!(std::io::ErrorKind::InvalidInput, "Cannot open a directory"));
        }
        lock_file(&mut cfd, &mut file_entry)?;

        let start_block = file_entry.start_block;
        let handle = self.s.file.open_existing_block(start_block)?;

        Ok(FileHandle {
            header: Box::new(file_entry),
            data: FileDataReader::LongFile(handle),
            s: &self.s,
            modified: false,
        })
    }

    /// Create a new file and open it.
    pub fn create_file<'a, 's>(&'a self, path: impl ResolveToNew<'s>) -> std::io::Result<FileHandle<'a>> {
        let (parent_dir, file_name) = path.resolve(self)?;
        if self.get_file_by_name(parent_dir, file_name)?.is_some() {
            return Err(const_error!(std::io::ErrorKind::AlreadyExists, "File already exists"));
        }
        let new_file_ref: FileRef = self.create_(parent_dir, file_name, false)?;
        self.open(new_file_ref)
    }

    pub fn create_directory<'s>(&self, path: impl ResolveToNew<'s>) -> std::io::Result<FileRef> {
        let (parent_dir, file_name) = path.resolve(self)?;
        if self.get_file_by_name(parent_dir, file_name)?.is_some() {
            return Err(const_error!(std::io::ErrorKind::AlreadyExists, "File already exists"));
        }
        self.create_(parent_dir, file_name, true)
    }

    fn create_(&self, parent: FileRef, name: &[u8], is_directory: bool) -> std::io::Result<FileRef> {
        let mut cfd = self.s.cfd.lock().unwrap();
        
        let parent_dir_entry = read_file_entry(&mut cfd, parent)?
            .ok_or(const_error!(std::io::ErrorKind::NotFound, "Parent directory not found"))?;
        assert!(parent_dir_entry.flags.is_dir() && parent_dir_entry.flags.is_not_empty());
        
        cfd.seek(std::io::SeekFrom::End(0))?;
        cfd.write_all(&[0; 128])?;
        let new_file_uid = cfd.stream_position()? / 128 - 1;
        let mut new_file_header = FileEntry::zeroed();
        new_file_header.uid = new_file_uid;
        new_file_header.flags.set_is_not_empty(true);
        new_file_header.flags.set_dir(is_directory);
        new_file_header.flags.set_readonly(parent_dir_entry.flags.is_readonly()); // unless specified otherwise, inherit readonly from parent directory
        new_file_header.parent_uid = parent.uid;
        if name.len() > 24 {
            new_file_header.flags.set_has_longname(true);
            let longname_location = self.s.dyndata.lock().unwrap().store_slice(name)?;
            new_file_header.shortname[0..16].copy_from_slice(&name[0..16]);
            new_file_header.shortname[16..24].copy_from_slice(&longname_location.to_le_bytes());
        } else {
            new_file_header.shortname[0..name.len()].copy_from_slice(name);
        }
        new_file_header.start_block = self.s.file.create_new_block()?.0;
        new_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        new_file_header.modify_time = new_file_header.create_time;
        if !is_directory { new_file_header.access_time = new_file_header.create_time; }
        cfd.seek(std::io::SeekFrom::Current(-128))?;
        cfd.write_all(bytemuck::bytes_of(&new_file_header))?;

        let mut directory_listing = self.s.file.open_existing_block(parent_dir_entry.start_block)?;
        directory_listing.seek(std::io::SeekFrom::End(0))?;
        directory_listing.write_all(&new_file_uid.to_le_bytes())?;

        Ok(FileRef { uid: new_file_uid })
    }

    /// Get information about a file. This operation is not exclusive.
    pub fn info(&self, file: impl ResolveToExisting) -> std::io::Result<Option<FileInfo>> {
        let file = match file.resolve(self)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut cfd = self.s.cfd.lock().unwrap();
        match read_file_entry(&mut cfd, file)? {
            None => Ok(None),
            Some(e) => Ok(Some(FileInfo::from_entry(e, &*self.s)?))
        }
    }

    pub fn dir_entries(&self, dir: impl ResolveToExisting) -> std::io::Result<Vec<FileRef>> {
        let dir = match dir.resolve(self)? {
            Some(d) => d,
            None => return Err(const_error!(std::io::ErrorKind::NotFound, "Directory not found")),
        };
        let mut cfd = self.s.cfd.lock().unwrap();
        
        let dir_entry = read_file_entry(&mut cfd, dir)?
            .ok_or(const_error!(std::io::ErrorKind::NotFound, "Directory not found"))?;

        let mut directory_listing = self.s.file.open_existing_block(dir_entry.start_block)?;
        let mut entries = Vec::new();
        let mut buf = [0u8; 8];
        while let Ok(n) = directory_listing.read(&mut buf) {
            if n < 8 { break; }
            let uid = u64::from_le_bytes(buf);
            if uid != 0 { entries.push(FileRef { uid }); }
        }
        Ok(entries)
    }

    pub fn root(&self) -> FileRef {
        FileRef { uid: 0 }
    }

    /// Use to move or rename a file or directory.
    /// This operation is exclusive and will fail if the file is open.
    pub fn move_<'s>(&self, path: impl ResolveToExisting, new_path: impl ResolveToNew<'s>) -> std::io::Result<FileRef> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let (new_parent_dir, new_name) = new_path.resolve(self)?;
        let mut cfd = self.s.cfd.lock().unwrap();
        let mut file_entry = read_file_entry(&mut cfd, file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        lock_file(&mut cfd, &mut file_entry)?;
        
        let old_parent_dir = file_entry.parent_uid;
        if old_parent_dir != new_parent_dir.uid {
            self.remove_file_in_directory_listing(&mut cfd, FileRef { uid: old_parent_dir }, file)?;
            let new_parent_dir_entry = read_file_entry(&mut cfd, new_parent_dir)?
                .ok_or(const_error!(std::io::ErrorKind::NotFound, "Parent directory not found"))?;
            let mut new_dir_listing = self.s.file.open_existing_block(new_parent_dir_entry.start_block)?;
            new_dir_listing.seek(std::io::SeekFrom::End(0))?;
            new_dir_listing.write_all(&file.uid.to_le_bytes())?;
            file_entry.parent_uid = new_parent_dir.uid;
        }
        
        if file_entry.flags.has_longname() {
            self.s.dyndata.lock().unwrap().free_slice(u64::from_le_bytes(file_entry.shortname[16..24].try_into().unwrap()))?;
        }
        if new_name.len() > 24 {
            file_entry.flags.set_has_longname(true);
            let longname_location = self.s.dyndata.lock().unwrap().store_slice(new_name)?;
            file_entry.shortname[0..16].copy_from_slice(&new_name[0..16]);
            file_entry.shortname[16..24].copy_from_slice(&longname_location.to_le_bytes());
        } else {
            file_entry.flags.set_has_longname(false);
            file_entry.shortname = [0; 24];
            file_entry.shortname[0..new_name.len()].copy_from_slice(new_name);
        }
        file_entry.flags.set_is_excl_lock(false);
        cfd.seek(std::io::SeekFrom::Start(file.uid * 128))?;
        cfd.write_all(bytemuck::bytes_of(&file_entry))?;
        Ok(file)
    }

    /// Delete a file.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn delete(&self, path: impl ResolveToExisting) -> std::io::Result<()> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let mut cfd = self.s.cfd.lock().unwrap();
        let mut file_entry = read_file_entry(&mut cfd, file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        lock_file(&mut cfd, &mut file_entry)?;
        if file_entry.flags.is_dir() {
            let mut directory_listing = self.s.file.open_existing_block(file_entry.start_block)?;
            let mut buf = [0u8; 8];
            while let Ok(n) = directory_listing.read(&mut buf) {
                if n < 8 { break; }
                if buf != [0; 8] {
                    return Err(const_error!(std::io::ErrorKind::Other, "Directory is not empty"));
                }
            }
        }
        if file_entry.flags.has_longname() {
            self.s.dyndata.lock().unwrap().free_slice(u64::from_le_bytes(file_entry.shortname[16..24].try_into().unwrap()))?;
        }
        self.remove_file_in_directory_listing(&mut cfd, FileRef { uid: file_entry.parent_uid }, file)?;
        // clear file_entry
        cfd.seek(std::io::SeekFrom::Start(file.uid * 128))?;
        cfd.write_all(&[0; 128])?;
        self.s.file.mark_garbage(file_entry.start_block)?;
        Ok(())
    }
        

    fn remove_file_in_directory_listing(&self, cfd: &mut std::sync::MutexGuard<'_, PFileBlockCachedHandle>, dir: FileRef, file: FileRef) -> std::io::Result<bool> {
        let dir_entry = read_file_entry(cfd, dir)?.unwrap();
        assert!(dir_entry.flags.is_dir());
        let mut directory_listing = self.s.file.open_existing_block(dir_entry.start_block)?;
        let mut buf = [0u8; 8];
        while let Ok(n) = directory_listing.read(&mut buf) {
            if n < 8 { break; }
            if u64::from_le_bytes(buf) == file.uid {
                directory_listing.seek(std::io::SeekFrom::Current(-8))?;
                directory_listing.write_all(&[0; 8])?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn get_file_by_name(&self, parent_dir: FileRef, filename: &[u8]) -> std::io::Result<Option<(FileRef, FileInfo)>> {
        let entries = self.dir_entries(parent_dir)?;
        for e in entries {
            if let Some(info) = self.info(e)? {
                if &*info.name == filename {
                    return Ok(Some((e, info)))
                }
            }
        }
        Ok(None)
    }

    fn _resolve_path(&self, mut path: &[u8], create_directories: bool, create_last_directory: bool) -> std::io::Result<Option<FileRef>> {
        if path.is_empty() || path == b"/" {
            return Ok(Some(self.root()));
        }
        if path_separator(&path[0]) {
            path = &path[1..];
        }
        if path_separator(&path[path.len() - 1]) {
            path = &path[..path.len() - 1]; // strip trailing slash
        }
        let (dir_path, file_name) = path_split_at_last_component(path);
        let mut dir = self.root();
        for component in dir_path.split(path_separator) {
            if component.is_empty() { continue; }
            match self.get_file_by_name(dir, component)? {
                Some((dref, dinfo)) => {
                    if !dinfo.is_directory {
                        return Err(const_error!(std::io::ErrorKind::InvalidInput, "Path component is not a directory"));
                    }
                    dir = dref;
                },
                None => {
                    if create_directories {
                        dir = self.create_(dir, component, true)?;
                    } else {
                        return Ok(None);
                    }
                },
            }
        };
        let res = self.get_file_by_name(dir, file_name)?.map(|x| x.0);
        if res.is_none() && create_last_directory {
            let new_dir = self.create_(dir, file_name, true)?;
            return Ok(Some(new_dir));
        }
        Ok(res)
    }

    pub fn resolve_path(&self, path: impl AsRef<[u8]>) -> std::io::Result<Option<FileRef>> {
        <&[u8] as sealed::Sealed1>::resolve(&path.as_ref(), self)
    }


    
}

/// must be called right after read_file_entry so the reader is in the right position to write the lock flag
fn lock_file(cfd: &mut std::sync::MutexGuard<'_, PFileBlockCachedHandle>, file_entry: &mut FileEntry) -> std::io::Result<()> {
    // byte-based OS-level lock here ?
    if file_entry.flags.is_excl_lock() {
        return Err(const_error!(std::io::ErrorKind::ResourceBusy, "File is currently locked"));
    }
    file_entry.flags.set_is_excl_lock(true);
    cfd.seek(std::io::SeekFrom::Current(-117))?;
    cfd.write_all(&[1])?;
    cfd.seek(std::io::SeekFrom::Current(116))?;
    cfd.flush()
}

fn read_file_entry(cfd: &mut std::sync::MutexGuard<'_, PFileBlockCachedHandle>, fr: FileRef) -> std::io::Result<Option<FileEntry>> {
    cfd.seek(std::io::SeekFrom::Start(fr.uid * 128))?;
    let mut file_entry = FileEntry::zeroed();
    let n = cfd.read(bytemuck::bytes_of_mut(&mut file_entry))?;
    if n < 128 { return Ok(None); }
    Ok(Some(file_entry))
}

fn path_separator(c: &u8) -> bool { *c == b'/' || *c == b'\\' }

fn path_split_at_last_component(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(path_separator) {
        Some(j) => (&path[..j], &path[j + 1..]),
        None => (&[], path),
    }
}

mod sealed {
    pub trait Sealed1 {
        fn resolve(&self, drive: &super::Drive) -> std::io::Result<Option<super::FileRef>>;
    }
    pub trait Sealed2<'a> : 'a {
        fn resolve(self, drive: &super::Drive) -> std::io::Result<(super::FileRef, &'a [u8])>;
    }
}

impl sealed::Sealed1 for FileRef {
    fn resolve(&self, _drive: &Drive) -> std::io::Result<Option<FileRef>> {
        Ok(Some(*self))
    }
}
impl ResolveToExisting for FileRef {}
impl sealed::Sealed1 for &FileHandle<'_> {
    fn resolve(&self, _: &self::Drive) -> std::io::Result<Option<self::FileRef>> {
        Ok(Some(self.get_ref()))
    }
}
impl ResolveToExisting for &FileHandle<'_> {}

impl<'a> sealed::Sealed2<'a> for (FileRef, &'a [u8]) {
    fn resolve(self, _: &self::Drive) -> std::io::Result<(self::FileRef, &'a [u8])> {
        Ok(self)
    }
}
impl<'a> ResolveToNew<'a> for (FileRef, &'a [u8]) {}

impl<T: AsRef<[u8]>> sealed::Sealed1 for T {
    fn resolve(&self, drive: &self::Drive) -> std::io::Result<Option<self::FileRef>> {
        drive._resolve_path(self.as_ref(), false, false)
    }
}
impl<T: AsRef<[u8]>> ResolveToExisting for T {}
impl<'a> sealed::Sealed2<'a> for &'a [u8] {
    fn resolve(self, drive: &self::Drive) -> std::io::Result<(self::FileRef, &'a [u8])> {
        let mut path = self;
        if path_separator(&path[0]) {
            path = &path[1..];
        }
        if path_separator(&path[path.len() - 1]) {
            path = &path[..path.len() - 1]; // strip trailing slash
        }
        let (dir_path, file_name) = path_split_at_last_component(path);
        if file_name.is_empty() {
            return Err(const_error!(std::io::ErrorKind::InvalidInput, "File name cannot be empty"));
        }
        let parent_dir = 
            drive._resolve_path(dir_path, true, true)?
            .unwrap(); // both arguments true => will be created if it doesn't exist
        Ok((parent_dir, file_name))
    }
}
impl<'a> ResolveToNew<'a> for &'a [u8] {}
impl<'a> sealed::Sealed2<'a> for &'a str {
    fn resolve(self, drive: &self::Drive) -> std::io::Result<(self::FileRef, &'a [u8])> {
        self.as_bytes().resolve(drive)
    }
}
impl<'a> ResolveToNew<'a> for &'a str {}

/// Types which may be used to reference an existing file/directory in the drive.
/// Implemented by `FileRef`, `&FileHandle`, and any path-like type that dereferences to a byte slice.
pub trait ResolveToExisting : sealed::Sealed1 {}
/// Types which reference a file/directory that may not exist yet.
/// Implemented by `&[u8]` and `&str` (representing the path), and `(FileRef, &[u8])` (representing an existing parent directory and the new file's name).
pub trait ResolveToNew<'a> : sealed::Sealed2<'a> {}


/// Dyndata is a storage space for short data optimization, long file names, and any short bytes that are not worth their own block.
/// It is a list of "slices", each prefixed with a varint length.
/// The length is encoded as unsigned LEB128, with the exception that the highest bit
/// of the first byte indicates "in use" (1) or "free" (0)
struct Dyndata {
    handle: PFileBlockCachedHandle,
    recently_freed: Vec<(u32, u32)>, // (offset, length) of recently freed slices, for reuse
}

const MAX_RECENTLY_FREED: usize = 16;

impl Dyndata {
    fn new(handle: PFileBlockCachedHandle) -> Self {
        Dyndata { handle, recently_freed: Vec::with_capacity(MAX_RECENTLY_FREED) }
    }

    fn store_slice(&mut self, data: &[u8]) -> std::io::Result<u64> {
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
        if data.len() < 64 {
            self.handle.write_all(&[0x80 | (data.len() as u8)])?;
            self.handle.write_all(data)?;
            Ok(pos)
        } else if data.len() < 8192 {
            self.handle.write_all(&[0xC0 | (data.len() & 0x3F) as u8,
                                         0x00 | ((data.len() >> 6) as u8)])?;
            self.handle.write_all(data)?;
            Ok(pos)
        } else if data.len() < 1048576 {
            self.handle.write_all(&[0xE0 | (data.len() & 0x3F) as u8,
                                         0x00 | ((data.len() >> 6) & 0x7F) as u8,
                                         0x00 | ((data.len() >> 13) as u8)])?;
            self.handle.write_all(data)?;
            Ok(pos)
        } else {
            Err(const_error!(std::io::ErrorKind::InvalidInput, "Data too large for dyndata"))
        }
    }

    fn get_slice(&mut self, offset: u64) -> std::io::Result<Vec<u8>> {
        let mut buf = [0u8; 3];
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        self.handle.read_exact(&mut buf)?;
        let len = varint_parse(buf);
        if len < 64 { // one-byte length
            let mut data = vec![0u8; len];
            if len > 0 {
                data[0] = buf[1];
                if len > 1 {
                    data[1] = buf[2];
                    if len > 2 {
                        self.handle.read_exact(&mut data[2..])?;
                    }
                }
            }
            Ok(data)
        } else if len < 8192 { // two-byte length
            let mut data = vec![0u8; len];
            if len > 0 {
                data[0] = buf[2];
                if len > 1 {
                    self.handle.read_exact(&mut data[1..])?;
                }
            }
            Ok(data)
        } else { // three-byte length
            let mut data = vec![0u8; len];
            if len > 0 {
                self.handle.read_exact(&mut data)?;
            }
            Ok(data)
        }
    }

    fn free_slice(&mut self, offset: u64) -> std::io::Result<()> {
        self.handle.seek(std::io::SeekFrom::Start(offset))?;
        let mut buf = [0u8; 3];
        self.handle.read_exact(&mut buf)?;
        buf[0] &= 0x7F; // clear the "in use" bit
        self.handle.seek(std::io::SeekFrom::Current(-1))?;
        self.handle.write_all(&buf)?;
        if self.recently_freed.len() < MAX_RECENTLY_FREED {
            self.recently_freed.push((offset as u32, varint_parse(buf) as u32));
        }
        Ok(())
    }
}

fn varint_parse(bytes: [u8; 3]) -> usize {
    if bytes[0] & 0x40 == 0 {
        (bytes[0] & 0x3F) as usize
    } else if bytes[1] & 0x80 == 0 {
        (((bytes[0] & 0x3F) as usize) | ((bytes[1] as usize) << 6)) as usize
    } else {
        (((bytes[0] & 0x3F) as usize) | ((bytes[1] as usize) << 6) | ((bytes[2] as usize) << 13)) as usize
    }
}


#[derive(Debug)]
pub struct FileInfo {
    pub name: Vec<u8>,
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

fn null_terminated_string(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    }
}

impl FileInfo {
    fn from_entry(value: FileEntry, s: &Shared) -> std::io::Result<Self> {
        let name = 
            if value.flags.has_longname() {
                let longname_location = u64::from_le_bytes(value.shortname[16..24].try_into().unwrap());
                let fullname = s.dyndata.lock().unwrap().get_slice(longname_location)?;
                debug_assert!(fullname[..16] == value.shortname[..16]);
                fullname
            } else {
                null_terminated_string(&value.shortname).to_vec()
            };
        let user_attrs =
            if value.user_attrs_pos != 0 {
                Some(s.dyndata.lock().unwrap().get_slice(value.user_attrs_pos)?)
            } else { None };
        Ok(FileInfo {
            name,
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

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern, NoUninit)]
struct FileEntry {
    uid: u64,
    flags: FileFlagsRaw,
    _reserved1: [u8; 4],
    parent_uid: u64,
    size: u64,
    start_block: u64,
    create_time: u64,
    modify_time: u64,
    access_time: u64,
    // if flags.longname is set:
    // - the full name is stored in dyndata
    // - last 8 bytes of this are the position
    // - first 16 bytes are the first 16 bytes of the name for quick comparison
    shortname: [u8; 24],
    // user-defined attributes, stored in dyndata. 0 = none
    user_attrs_pos: u64,
    _reserved2: [u8; 32],
}

const SDO_LIMIT: usize = 64;

bitfield! {
    #[derive(Clone, Copy, Zeroable, Pod)]
    #[repr(transparent)]
    struct FileFlagsRaw(u32);
    is_not_empty, set_is_not_empty : 0;
    is_dir, set_dir : 1;
    is_readonly, set_readonly : 2;

    //is_sdo, set_sdo : 11; // short data optimization: TODO
    has_longname, set_has_longname : 12;
    
    is_excl_lock, set_is_excl_lock : 24;
}

enum FileDataReader {
    LongFile(PFileBlockHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// A non-owning reference to a file. The file may or may not exist.
pub struct FileRef { pub(crate) uid: u64 }

/// An owning reference to a properly opened file.
pub struct FileHandle<'dr> {
    header: Box<FileEntry>,
    data: FileDataReader,
    s: &'dr Shared,
    modified: bool,
}

impl FileHandle<'_> {
    fn write_file_entry_to_disk(&self) -> std::io::Result<()> {
        let mut cfd = self.s.cfd.lock().unwrap();
        cfd.seek(std::io::SeekFrom::Start(self.header.uid * 128))?;
        cfd.write_all(bytemuck::bytes_of(&*self.header))
    }

    pub fn file_size(&self) -> u64 {
        self.header.size
    }

    pub fn parent_dir(&self) -> FileRef {
        FileRef { uid: self.header.parent_uid }
    }

    pub fn name(&self) -> Cow<'_, [u8]> {
        if self.header.flags.has_longname() {
            let longname_location = u64::from_le_bytes(self.header.shortname[16..24].try_into().unwrap());
            let fullname = self.s.dyndata.lock().unwrap().get_slice(longname_location).unwrap();
            debug_assert!(fullname[..16] == self.header.shortname[..16]);
            Cow::Owned(fullname)
        } else {
            Cow::Borrowed(null_terminated_string(&self.header.shortname))
        }
    }

    pub fn set_name(&mut self, name: &[u8]) -> std::io::Result<()> {
        if self.header.flags.has_longname() {
            self.s.dyndata.lock().unwrap().free_slice(u64::from_le_bytes(self.header.shortname[16..24].try_into().unwrap()))?;
        }
        if name.len() > 24 {
            self.header.flags.set_has_longname(true);
            let longname_location = self.s.dyndata.lock().unwrap().store_slice(name).unwrap();
            self.header.shortname[0..16].copy_from_slice(&name[0..16]);
            self.header.shortname[16..24].copy_from_slice(&longname_location.to_le_bytes());
        } else {
            self.header.flags.set_has_longname(false);
            self.header.shortname = [0; 24];
            self.header.shortname[..name.len()].copy_from_slice(name);
        }
        self.modified = true;
        Ok(())
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
        FileInfo::from_entry(*self.header, self.s)
    }

    pub fn user_attrs(&self) -> std::io::Result<Option<Vec<u8>>> {
        if self.header.user_attrs_pos != 0 {
            Ok(Some(self.s.dyndata.lock().unwrap().get_slice(self.header.user_attrs_pos)?))
        } else { Ok(None) }
    }

    pub fn set_user_attrs(&mut self, attrs: &[u8]) -> std::io::Result<()> {
        if self.header.user_attrs_pos != 0 {
            self.s.dyndata.lock().unwrap().free_slice(self.header.user_attrs_pos)?;
        }
        if attrs.is_empty() {
            self.header.user_attrs_pos = 0;
        } else {
            let pos = self.s.dyndata.lock().unwrap().store_slice(attrs)?;
            self.header.user_attrs_pos = pos;
        }
        Ok(())
    }

    pub fn set_len(&mut self, size: u64) -> std::io::Result<()> {
        if self.header.flags.is_readonly() {
            return Err(const_error!(std::io::ErrorKind::PermissionDenied, "File is read-only"));
        }
        let h = match &mut self.data {
            FileDataReader::LongFile(h) => h,
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
}

impl std::io::Read for FileHandle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.read(buf),
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
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.flush(),
        }?;
        self.header.access_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        self.write_file_entry_to_disk()
    }
}

impl std::io::Seek for FileHandle<'_> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match &mut self.data {
            FileDataReader::LongFile(h) => h.seek(pos),
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
        }
        // update file metadata as neccessary
        self.header.access_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        if self.modified {
            self.header.modify_time = self.header.access_time;
        }
        self.header.flags.set_is_excl_lock(false);
        let _ = self.write_file_entry_to_disk();
    }
}