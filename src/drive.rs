use std::{io::{Read, Seek, Write, const_error}, sync::Mutex};

use bitfield::bitfield;
use bytemuck::{AnyBitPattern, NoUninit, Pod, Zeroable};

use crate::paging::{PFileBlockCachedHandle, PFileBlockHandle, PagedFile};

#[repr(C)]
#[derive(Clone, Copy)]
struct DriveHeader {
    _pad: [u8; 248],
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
    dyndata: Mutex<PFileBlockCachedHandle>,
}

pub struct Drive {
    s: Box<Shared>,
}

impl Drive {
    pub fn new_create(source: PagedFile) -> std::io::Result<Self> {
        let (n, mut drive_header) = source.create_new_block()?;
        assert!(n == DRIVE_HEADER_BLOCK);
        let (n, mut cfd) = source.create_new_block_cached()?;
        assert!(n == CENTRAL_FILE_DIRECTORY_BLOCK);
        let (n, dyndata) = source.create_new_block_cached()?;
        assert!(n == DYNDATA_BLOCK);
        let dr_header = DriveHeader { _pad: [0; 248] }; // TODO
        drive_header.write_all(bytemuck::bytes_of(&dr_header))?;
        // create root dir
        let mut root_file_header = FileEntry::zeroed();
        root_file_header.flags.set_is_not_empty(true);
        root_file_header.flags.set_dir(true);
        root_file_header.start_block = source.create_new_block()?.0;
        root_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        cfd.write_all(bytemuck::bytes_of(&root_file_header))?;

        Ok(Drive { s: Box::new(Shared { file: source, cfd: Mutex::new(cfd), dyndata: Mutex::new(dyndata) }) })
    }

    pub fn new_existing(source: PagedFile) -> std::io::Result<Self> {
        let _drive_header = source.open_existing_block(DRIVE_HEADER_BLOCK)?;
        // TODO: read and validate drive header
        let cfd = source.open_existing_block_cached(CENTRAL_FILE_DIRECTORY_BLOCK)?;
        let dyndata = source.open_existing_block_cached(DYNDATA_BLOCK)?;
        Ok(Drive { s: Box::new(Shared { file: source, cfd: Mutex::new(cfd), dyndata: Mutex::new(dyndata) }) })
    }

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
        assert!(name.len() <= 24);
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
        new_file_header.shortname[..name.len()].copy_from_slice(name);
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

    pub fn info(&self, file: impl ResolveToExisting) -> std::io::Result<Option<FileInfo>> {
        let file = match file.resolve(self)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut cfd = self.s.cfd.lock().unwrap();
        Ok(read_file_entry(&mut cfd, file)?.map(FileInfo::from))
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
            entries.push(FileRef { uid });
        }
        Ok(entries)
    }

    pub fn root(&self) -> FileRef {
        FileRef { uid: 0 }
    }

    /// Use to move or rename a file or directory.
    pub fn move_<'s>(&self, path: impl ResolveToExisting, new_path: impl ResolveToNew<'s>) -> std::io::Result<FileRef> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let (new_parent_dir, new_name) = new_path.resolve(self)?;
        assert!(new_name.len() <= 24);
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
        
        file_entry.shortname = [0; 24];
        file_entry.shortname[..new_name.len()].copy_from_slice(new_name);
        file_entry.flags.set_is_excl_lock(false);
        cfd.seek(std::io::SeekFrom::Start(file.uid * 128))?;
        cfd.write_all(bytemuck::bytes_of(&file_entry))?;
        Ok(file)
    }

    pub fn delete(&self, path: impl ResolveToExisting) -> std::io::Result<()> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let mut cfd = self.s.cfd.lock().unwrap();
        let mut file_entry = read_file_entry(&mut cfd, file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        lock_file(&mut cfd, &mut file_entry)?;
        if file_entry.flags.is_dir() {
            let n = self.s.file.open_existing_block(file_entry.start_block)?.seek(std::io::SeekFrom::Start(1))?;
            if n > 0 {
                return Err(const_error!(std::io::ErrorKind::Other, "Directory is not empty"));
            }
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
        let mut found = false;
        while let Ok(n) = directory_listing.read(&mut buf) {
            if n < 8 { break; }
            if u64::from_le_bytes(buf) == file.uid {
                found = true;
                continue;
            }
            if found {
                directory_listing.seek(std::io::SeekFrom::Current(-16))?;
                directory_listing.write_all(&buf)?;
                directory_listing.seek(std::io::SeekFrom::Current(8))?;
            }
        }
        directory_listing.seek(std::io::SeekFrom::End(-8))?;
        directory_listing.shrink()?;
        Ok(found)
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

    fn resolve_path(&self, mut path: &[u8], create_directories: bool, create_last_directory: bool) -> std::io::Result<Option<FileRef>> {
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

    
}

/// must be called right after read_file_entry so the reader is in the right position to write the lock flag
fn lock_file(cfd: &mut std::sync::MutexGuard<'_, PFileBlockCachedHandle>, file_entry: &mut FileEntry) -> std::io::Result<()> {
    // byte-based OS-level lock here ?
    if file_entry.flags.is_excl_lock() {
        return Err(const_error!(std::io::ErrorKind::Other, "File is currently locked"));
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
        drive.resolve_path(self.as_ref(), false, false)
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
            drive.resolve_path(dir_path, true, true)?
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
}

fn null_terminated_string(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(pos) => &bytes[..pos],
        None => bytes,
    }
}

impl From<FileEntry> for FileInfo {
    fn from(value: FileEntry) -> Self {
        FileInfo {
            name: null_terminated_string(&value.shortname).to_vec(),
            is_directory: value.flags.is_dir(),
            is_readonly: value.flags.is_readonly(),
            parent: FileRef { uid: value.parent_uid },
            size: value.size,
            create_time: value.create_time,
            modify_time: if !value.flags.is_dir() { Some(value.modify_time) } else { None },
            access_time: if !value.flags.is_dir() { Some(value.access_time) } else { None },
        }
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
    shortname: [u8; 24],
    _reserved2: [u8; 32],
    _reserved3: [u8; 8],
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
    //has_longname, set_has_longname : 12;: TODO
    
    is_excl_lock, set_is_excl_lock : 24;
}

enum FileDataReader {
    LongFile(PFileBlockHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// A non-owning reference to a file. The file may or may not exist.
pub struct FileRef { uid: u64 }

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

    pub fn name(&self) -> &[u8] {
        null_terminated_string(&self.header.shortname)
    }

    pub fn set_name(&mut self, name: &[u8]) {
        assert!(name.len() <= 24, "File name too long");
        self.header.shortname = [0; 24];
        self.header.shortname[..name.len()].copy_from_slice(name);
        self.modified = true;
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

    pub fn modify_time(&self) -> u64 {
        self.header.modify_time
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