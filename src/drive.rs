use std::{io::{Read, Seek, Write}, sync::Mutex};

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
const FILE_DIRECTORY_BLOCK: u64 = 1;
const DYNDATA_BLOCK: u64 = 2;

struct Shared {
    file: PagedFile,
    file_directory: Mutex<PFileBlockCachedHandle>,
    dyndata: Mutex<PFileBlockCachedHandle>,
}

pub struct Drive {
    s: Box<Shared>,
}

impl Drive {
    pub fn new_create(source: PagedFile) -> std::io::Result<Self> {
        let (n, mut drive_header) = source.create_new_block()?;
        assert!(n == DRIVE_HEADER_BLOCK);
        let (n, mut file_directory) = source.create_new_block_cached()?;
        assert!(n == FILE_DIRECTORY_BLOCK);
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
        file_directory.write_all(bytemuck::bytes_of(&root_file_header))?;

        Ok(Drive { s: Box::new(Shared { file: source, file_directory: Mutex::new(file_directory), dyndata: Mutex::new(dyndata) }) })
    }

    pub fn new_existing(source: PagedFile) -> std::io::Result<Self> {
        let _drive_header = source.open_existing_block(DRIVE_HEADER_BLOCK)?;
        // TODO: read and validate drive header
        let file_directory = source.open_existing_block_cached(FILE_DIRECTORY_BLOCK)?;
        let dyndata = source.open_existing_block_cached(DYNDATA_BLOCK)?;
        Ok(Drive { s: Box::new(Shared { file: source, file_directory: Mutex::new(file_directory), dyndata: Mutex::new(dyndata) }) })
    }

    pub fn open<'a>(&'a self, file: FileRef) -> std::io::Result<FileHandle<'a>> {
        let mut file_directory = self.s.file_directory.lock().unwrap();
        let p = file_directory.seek(std::io::SeekFrom::Start(file.uid * 128))?;
        if !self.s.file.as_ref().try_lock_part(p, 128, false, false)? {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "This file is currently in use by another process"));
        }
        
        let mut file_entry = FileEntry::zeroed();
        file_directory.read_exact(bytemuck::bytes_of_mut(&mut file_entry))?;
        
        assert!(file_entry.flags.is_not_empty());
        assert!(file_entry.uid == file.uid);
        if file_entry.flags.is_dir() {
            todo!("open_file: file is a directory");
        }
        if file_entry.flags.is_excl_lock() {
            todo!("open_file: file is currently locked");
        }
        file_entry.flags.set_is_excl_lock(true);
        // mark the file as locked
        file_directory.seek(std::io::SeekFrom::Current(-117))?;
        file_directory.write_all(&[1])?;

        let start_block = file_entry.start_block;
        let handle = self.s.file.open_existing_block(start_block)?;

        Ok(FileHandle {
            header: Box::new(file_entry),
            data: FileDataReader::LongFile(handle),
            s: &self.s,
            modified: false,
        })
    }

    pub fn create_file(&self, parent: FileRef, name: &[u8]) -> std::io::Result<FileHandle> {
        // todo: check file with the same name doesn't already exist in the parent directory
        assert!(!name.is_empty());
        let new_file_ref = self.create_(parent, name, false)?;
        self.open(new_file_ref)
    }

    pub fn create_directory(&self, parent: FileRef, name: &[u8]) -> std::io::Result<FileRef> {
        assert!(!name.is_empty());
        self.create_(parent, name, true)
    }

    fn create_(&self, parent: FileRef, name: &[u8], is_directory: bool) -> std::io::Result<FileRef> {
        assert!(name.len() <= 24);

        let mut file_directory = self.s.file_directory.lock().unwrap();
        file_directory.seek(std::io::SeekFrom::Start(parent.uid * 128))?;
        let mut parent_dir_entry = FileEntry::zeroed();
        file_directory.read_exact(bytemuck::bytes_of_mut(&mut parent_dir_entry))?;
        assert!(parent_dir_entry.flags.is_dir() && parent_dir_entry.flags.is_not_empty());
        
        file_directory.seek(std::io::SeekFrom::End(0))?;
        file_directory.write_all(&[0; 128])?;
        let new_file_uid = file_directory.stream_position()? / 128 - 1;
        let mut new_file_header = FileEntry::zeroed();
        new_file_header.uid = new_file_uid;
        new_file_header.flags.set_is_not_empty(true);
        new_file_header.flags.set_dir(is_directory);
        new_file_header.parent_uid = parent.uid;
        new_file_header.shortname[..name.len()].copy_from_slice(name);
        new_file_header.start_block = self.s.file.create_new_block()?.0;
        new_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        new_file_header.modify_time = new_file_header.create_time;
        if !is_directory { new_file_header.access_time = new_file_header.create_time; }
        file_directory.seek(std::io::SeekFrom::Current(-128))?;
        file_directory.write_all(bytemuck::bytes_of(&new_file_header))?;

        let mut directory_listing = self.s.file.open_existing_block(parent_dir_entry.start_block)?;
        directory_listing.seek(std::io::SeekFrom::End(0))?;
        directory_listing.write_all(&new_file_uid.to_le_bytes())?;

        Ok(FileRef { uid: new_file_uid })
    }

    pub fn info(&self, file: FileRef) -> std::io::Result<Option<FileInfo>> {
        let mut file_directory = self.s.file_directory.lock().unwrap();
        file_directory.seek(std::io::SeekFrom::Start(file.uid * 128))?;
        let mut file_entry = FileEntry::zeroed();
        let n = file_directory.read(bytemuck::bytes_of_mut(&mut file_entry))?;
        if n < 128 { return Ok(None); }
        if !file_entry.flags.is_not_empty() { return Ok(None); }
        Ok(Some(FileInfo::from(&file_entry)))
    }

    pub fn dir_entries(&self, dir: FileRef) -> std::io::Result<Vec<FileRef>> {
        let mut file_directory = self.s.file_directory.lock().unwrap();
        file_directory.seek(std::io::SeekFrom::Start(dir.uid * 128))?;
        let mut dir_entry = FileEntry::zeroed();
        file_directory.read_exact(bytemuck::bytes_of_mut(&mut dir_entry))?;
        assert!(dir_entry.flags.is_dir() && dir_entry.flags.is_not_empty());

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
}

#[derive(Debug)]
pub struct FileInfo {
    pub name: Vec<u8>,
    pub is_directory: bool,
    pub parent: FileRef,
    pub size: u64,
    pub create_time: u64,
    pub modify_time: u64,
    pub access_time: u64,
}

impl<'a> From<&'a FileEntry> for FileInfo {
    fn from(value: &'a FileEntry) -> Self {
        FileInfo {
            name: value.shortname.iter().take_while(|c| **c != 0).map(|&c| c).collect(),
            is_directory: value.flags.is_dir(),
            parent: FileRef { uid: value.parent_uid },
            size: value.size,
            create_time: value.create_time,
            modify_time: value.modify_time,
            access_time: value.access_time,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern, NoUninit)]
struct FileEntry {
    uid: u64,
    flags: FileFlags,
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
    struct FileFlags(u32);
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
        let mut file_directory = self.s.file_directory.lock().unwrap();
        file_directory.seek(std::io::SeekFrom::Start(self.header.uid * 128))?;
        file_directory.write_all(bytemuck::bytes_of(&*self.header))
    }

    pub fn file_size(&self) -> u64 {
        self.header.size
    }

    pub fn parent_dir(&self) -> FileRef {
        FileRef { uid: self.header.parent_uid }
    }

    pub fn name(&self) -> &[u8] {
        match self.header.shortname.iter().enumerate().find(|(_, c)| **c == 0) {
            Some((pos, _)) => &self.header.shortname[..pos],
            None => &self.header.shortname,
        }
    }

    pub fn set_name(&mut self, name: &[u8]) {
        assert!(name.len() <= 24, "File name too long");
        self.header.shortname = [0; 24];
        self.header.shortname[..name.len()].copy_from_slice(name);
    }

    pub fn get_ref(&self) -> FileRef {
        FileRef { uid: self.header.uid }
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