use std::{io::{Read, Seek, Write, const_error}, sync::Mutex};
use bytemuck::{AnyBitPattern, NoUninit, Zeroable};
use crate::{fs::{pcache::PathCache, *}, paging::PagedFile, util::{self, FileLike, null_terminated_string}};

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

pub(crate) struct Shared {
    pub(crate) file: PagedFile,
    pub(crate) cft: Mutex<Cft>,
    pub(crate) dyndata: Mutex<Dyndata>,
    pub(crate) path_cache: PathCache,
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
        let (n, mut cft) = source.create_new_block_cached()?;
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
        let mut root_file_header = FileRecord::zeroed();
        root_file_header.flags.set_is_not_empty(true);
        root_file_header.flags.set_dir(true);
        root_file_header.start_block = source.create_new_block()?.0;
        root_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        cft.write_all(bytemuck::bytes_of(&root_file_header))?;

        Ok(Drive { s: Box::new(Shared { file: source, cft: Mutex::new(Cft::new(cft)), dyndata: Mutex::new(dyndata), path_cache: PathCache::new() }), header: Box::new(dr_header) })
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
        let cft = source.open_existing_block_cached(CENTRAL_FILE_DIRECTORY_BLOCK)?;
        let dyndata = source.open_existing_block_cached(DYNDATA_BLOCK)?;
        Ok(Drive { s: Box::new(Shared { file: source, cft: Mutex::new(Cft::new(cft)), dyndata: Mutex::new(Dyndata::new(dyndata)), path_cache: PathCache::new() }), header: Box::new(*drive_header) })
    }

    /// Open a file for reading and writing.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn open<'a>(&'a self, file: impl ResolveToExisting) -> std::io::Result<FileHandle<'a>> {
        let file = file.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let mut cft = self.s.cft.lock().unwrap();        
        let mut file_record = 
            cft.read_file_record(file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        
        assert!(file_record.flags.is_not_empty());
        assert!(file_record.uid == file.uid);
        if file_record.flags.is_dir() {
            return Err(const_error!(std::io::ErrorKind::InvalidInput, "Cannot open a directory"));
        }
        cft.lock_file(&mut file_record)?;
        FileHandle::new(file_record, &self.s)
    }

    /// Create a new file and open it.
    /// 
    /// This method will create missing parent directories if they do not exist.
    /// It will fail if the file already exists.
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
        let mut cft = self.s.cft.lock().unwrap();
        
        let mut parent_dir_record = cft.read_file_record(parent)?
            .ok_or(const_error!(std::io::ErrorKind::NotFound, "Parent directory not found"))?;
        assert!(parent_dir_record.flags.is_dir() && parent_dir_record.flags.is_not_empty());
        
        let mut new_file_header = cft.alloc_new_file_record()?;
        new_file_header.flags.set_is_not_empty(true);
        new_file_header.flags.set_dir(is_directory);
        new_file_header.flags.set_readonly(parent_dir_record.flags.is_readonly()); // unless specified otherwise, inherit readonly from parent directory
        new_file_header.parent_uid = parent.uid;
        if name.len() > 24 {
            new_file_header.flags.set_has_longname(true);
            let longname_location = self.s.dyndata.lock().unwrap().store_slice(name)?;
            new_file_header.shortname[0..16].copy_from_slice(&name[0..16]);
            new_file_header.shortname[16..24].copy_from_slice(&longname_location.to_le_bytes());
        } else {
            new_file_header.shortname[0..name.len()].copy_from_slice(name);
        }
        new_file_header.flags.set_sdo(true);
        new_file_header.start_block = 0; // sdo: lazy block allocation, wait for first write
        new_file_header.create_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        new_file_header.modify_time = new_file_header.create_time;
        if !is_directory { new_file_header.access_time = new_file_header.create_time; }
        cft.write_file_record(&new_file_header)?;

        self.add_file_to_directory_listing(&mut parent_dir_record, FileRef { uid: new_file_header.uid })?;
        cft.write_file_record(&parent_dir_record)?;

        Ok(FileRef { uid: new_file_header.uid })
    }

    /// Get information about a file. This operation is not exclusive.
    pub fn info(&self, file: impl ResolveToExisting) -> std::io::Result<Option<FileInfo>> {
        let file = match file.resolve(self)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut cft = self.s.cft.lock().unwrap();
        match cft.read_file_record(file)? {
            None => Ok(None),
            Some(e) => {
                if !e.flags.is_not_empty() {
                    return Ok(None);
                }
                Ok(Some(FileInfo::from_record(e, &*self.s)?))
            }
        }
    }

    pub fn dir_entries(&self, dir: impl ResolveToExisting) -> std::io::Result<Vec<FileRef>> {
        let dir = match dir.resolve(self)? {
            Some(d) => d,
            None => return Err(const_error!(std::io::ErrorKind::NotFound, "Directory not found")),
        };
        let mut cft = self.s.cft.lock().unwrap();
        let dir_record = cft.read_file_record(dir)?
            .ok_or(const_error!(std::io::ErrorKind::NotFound, "Directory not found"))?;
        self.dir_entries_(&dir_record, usize::MAX)
    }

    fn dir_entries_(&self, directory: &FileRecord, limit: usize) -> std::io::Result<Vec<FileRef>> {
        if !directory.flags.is_not_empty() {
            return Err(const_error!(std::io::ErrorKind::NotFound, "Directory not found"));
        }
        if directory.flags.is_sdo() {
            if directory.start_block == 0 { return Ok(Vec::new()); }
            let listing = self.s.dyndata.lock().unwrap().get_slice(directory.start_block)?;
            debug_assert!(listing.len() == SDO_DIR_SIZE);
            let mut entries = Vec::new();
            for i in (0..SDO_DIR_SIZE).step_by(8) {
                if entries.len() >= limit { break; }
                let uid = u64::from_le_bytes(listing[i..i+8].try_into().unwrap());
                if uid != 0 { entries.push(FileRef { uid }); }
            }
            Ok(entries)
        } else {
            let mut directory_listing = self.s.file.open_existing_block(directory.start_block)?;
            let mut entries = Vec::new();
            let mut buf = [0u8; 8];
            while let Ok(n) = directory_listing.read(&mut buf) {
                if n < 8 { break; }
                if entries.len() >= limit { break; }
                let uid = u64::from_le_bytes(buf);
                if uid != 0 { entries.push(FileRef { uid }); }
            }
            Ok(entries)
        }
    }

    pub fn root(&self) -> FileRef {
        FileRef { uid: 0 }
    }

    /// Use to move or rename a file or directory.
    /// This operation is exclusive and will fail if the file is open.
    /// 
    /// Similarly to create, this operation will create missing parent directories if they do not exist and
    /// it will fail if the new path already exists.
    pub fn move_<'s>(&self, path: impl ResolveToExisting, new_path: impl ResolveToNew<'s>) -> std::io::Result<FileRef> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let (new_parent_dir, new_name) = new_path.resolve(self)?;
        if self.get_file_by_name(new_parent_dir, new_name)?.is_some() {
            return Err(const_error!(std::io::ErrorKind::AlreadyExists, "File already exists"));
        }
        let mut cft = self.s.cft.lock().unwrap();
        let mut file_record = cft.read_file_record(file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        cft.lock_file(&mut file_record)?;
        
        let old_parent_dir = file_record.parent_uid;
        if old_parent_dir != new_parent_dir.uid {
            let old_parent_dir_record = cft.read_file_record(FileRef { uid: old_parent_dir })?.unwrap();
            self.remove_file_in_directory_listing(&old_parent_dir_record, file)?;
            let mut new_parent_dir_record = cft.read_file_record(new_parent_dir)?
                .ok_or(const_error!(std::io::ErrorKind::NotFound, "Parent directory not found"))?;
            self.add_file_to_directory_listing(&mut new_parent_dir_record, file)?;
            cft.write_file_record(&new_parent_dir_record)?;
            file_record.parent_uid = new_parent_dir.uid;
        }
        std::mem::drop(cft); // prevent deadlock
        self.s.path_cache.drop(old_parent_dir, file_record.get_filename(&self.s)?.as_ref());
        
        file_record.flags.set_is_excl_lock(false);
        let mut fh = FileHandle::new(file_record, &self.s)?;
        fh.set_name(new_name)?;
        fh.flush()?; // writes the file record to disk
        fh.close()?;
        Ok(file)
    }

    /// Delete a file.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn delete(&self, path: impl ResolveToExisting) -> std::io::Result<()> {
        let file = path.resolve(self)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        let mut cft = self.s.cft.lock().unwrap();
        let mut file_record = cft.read_file_record(file)?.ok_or(const_error!(std::io::ErrorKind::NotFound, "File not found"))?;
        cft.lock_file(&mut file_record)?; // make sure the file is not open
        if file_record.flags.is_dir() {
            if !self.dir_entries_(&file_record, 1)?.is_empty() {
                return Err(const_error!(std::io::ErrorKind::InvalidInput, "Directory is not empty"));
            }
        }
        if file_record.flags.has_longname() {
            self.s.dyndata.lock().unwrap().try_update_slice(
                u64::from_le_bytes(file_record.shortname[16..24].try_into().unwrap()), &[])?;
        }
        let parent_dir_record = cft.read_file_record(FileRef { uid: file_record.parent_uid })?.unwrap();
        self.remove_file_in_directory_listing(&parent_dir_record, file)?;
        self.s.path_cache.drop(file_record.parent_uid, file_record.get_filename(&self.s)?.as_ref());
        // clear file_record
        cft.delete_file_record(file)?;
        self.s.file.mark_garbage(file_record.start_block)?;
        Ok(())
    }
        

    fn remove_file_in_directory_listing(&self, dir_record: &FileRecord, file: FileRef) -> std::io::Result<bool> {
        if dir_record.flags.is_sdo() {
            if dir_record.start_block == 0 { return Ok(false); }
            let mut dd = self.s.dyndata.lock().unwrap();
            let mut listing = dd.get_slice(dir_record.start_block)?;
            debug_assert!(listing.len() == SDO_DIR_SIZE);
            for i in (0..SDO_DIR_SIZE).step_by(8) {
                if &listing[i..i+8] == &file.uid.to_le_bytes() {
                    listing[i..i+8].copy_from_slice(&[0; 8]);
                    let p = dd.try_update_slice(dir_record.start_block, &listing)?;
                    debug_assert!(p == dir_record.start_block);
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            let mut directory_listing = self.s.file.open_existing_block(dir_record.start_block)?;
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
    }

    fn add_file_to_directory_listing(&self, dir_record: &mut FileRecord, file: FileRef) -> std::io::Result<()> {
        if dir_record.flags.is_sdo() {
            if dir_record.start_block == 0 {
                let listing = [file.uid, 0u64, 0, 0, 0];
                dir_record.start_block = self.s.dyndata.lock().unwrap().store_slice(bytemuck::bytes_of(&listing))?;
                return Ok(())
            }
            let mut dd = self.s.dyndata.lock().unwrap();
            let mut listing = dd.get_slice(dir_record.start_block)?;
            debug_assert!(listing.len() == SDO_DIR_SIZE);
            for i in (0..SDO_DIR_SIZE).step_by(8) {
                if &listing[i..i+8] == &[0; 8] {
                    listing[i..i+8].copy_from_slice(&file.uid.to_le_bytes());
                    let p = dd.try_update_slice(dir_record.start_block, &listing)?;
                    debug_assert!(p == dir_record.start_block);
                    return Ok(())
                }
            }
            // no empty slot found, need to allocate a new block and move the listing there
            dd.try_update_slice(dir_record.start_block, &[])?;
            let (new_block, mut new_listing) = self.s.file.create_new_block()?;
            new_listing.write_all(&listing)?;
            new_listing.write_all(&file.uid.to_le_bytes())?;
            dir_record.start_block = new_block;
            dir_record.flags.set_sdo(false);
            Ok(())
        } else {
            let mut directory_listing = self.s.file.open_existing_block(dir_record.start_block)?;
            directory_listing.seek(std::io::SeekFrom::End(0))?;
            directory_listing.write_all(&file.uid.to_le_bytes())
        }
    }

    fn get_file_by_name(&self, parent_dir: FileRef, filename: &[u8]) -> std::io::Result<Option<(FileRef, Option<FileInfo>)>> {
        if let Some(cached_uid) = self.s.path_cache.lookup(parent_dir.uid, filename) {
            println!("get_file_by_name({:?}, {:?}) [cached]", parent_dir, filename);
            return Ok(Some((FileRef { uid: cached_uid }, None)));
        }
        println!("get_file_by_name({:?}, {:?})", parent_dir, filename);
        let entries = self.dir_entries(parent_dir)?;
        for e in entries {
            if let Some(info) = self.info(e)? {
                if &*info.name == filename {
                    self.s.path_cache.store(parent_dir.uid, filename, e.uid);
                    return Ok(Some((e, Some(info))));
                }
            }
        }
        Ok(None)
    }

    fn _resolve_path(&self, path: &[u8], create_directories: bool, create_last_directory: bool) -> std::io::Result<Option<FileRef>> {
        let mut path_components = util::path_components(path);
        let file_name = match path_components.next_back() {
            None => return Ok(Some(self.root())), // no path components = root dir
            Some(x) => x,
        };
        let mut dir = self.root();
        for component in path_components {
            match self.get_file_by_name(dir, component)? {
                Some((dref, dinfo)) => {
                    let dinfo = dinfo.map_or_else(|| self.info(dref), |i| Ok(Some(i)))?.unwrap();
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

    pub fn drive_stats(&self) -> std::io::Result<DriveStats> {
        let page_size = self.s.file.page_size();
        let used_pages = self.s.file.used_pages()?;
        let drive_name = null_terminated_string(&self.header.drive_name).to_vec().into_boxed_slice();
        let drive_owner = null_terminated_string(&self.header.drive_owner).to_vec().into_boxed_slice();
        Ok(DriveStats { page_size, used_pages, drive_name, drive_owner })
    }
}

pub struct DriveStats {
    pub page_size: u64,
    pub used_pages: u64,
    pub drive_name: Box<[u8]>,
    pub drive_owner: Box<[u8]>,
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
        let mut comps = util::path_components(self);
        let file_name = match comps.next_back() {
            None => return Err(const_error!(std::io::ErrorKind::InvalidInput, "File name cannot be empty")),
            Some(x) => x
        };
        let parent_dir = 
            drive._resolve_path(comps.remaining(), true, true)?
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
// needed to byte literals can be used
impl<'a, const N: usize> sealed::Sealed2<'a> for &'a [u8; N] {
    fn resolve(self, drive: &self::Drive) -> std::io::Result<(self::FileRef, &'a [u8])> {
        self.as_slice().resolve(drive)
    }
}
impl<'a, const N: usize> ResolveToNew<'a> for &'a [u8; N] {}

/// Types which may be used to reference an existing file/directory in the drive.
/// Implemented by `FileRef`, `&FileHandle`, and any path-like type that dereferences to a byte slice.
pub trait ResolveToExisting : sealed::Sealed1 {}
/// Types which reference a file/directory that may not exist yet.
/// Implemented by `&[u8]` and `&str` (representing the path), and `(FileRef, &[u8])` (representing an existing parent directory and the new file's name).
pub trait ResolveToNew<'a> : sealed::Sealed2<'a> {}

const SDO_DIR_SIZE: usize = 40; // 5 files, 8 bytes each

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// A non-owning reference to a file. The file may or may not exist.
pub struct FileRef { pub(crate) uid: u64 }

impl From<FileRef> for u64 {
    fn from(f: FileRef) -> Self { f.uid }
}
impl From<u64> for FileRef {
    fn from(uid: u64) -> Self { FileRef { uid } }
}
