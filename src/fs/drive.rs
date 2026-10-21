use std::{borrow::Cow, collections::HashSet, io::{Read, Seek, Write}, sync::Mutex};
use bytemuck::{AnyBitPattern, NoUninit, Zeroable};
use crate::{fs::{pcache::PathCache, *}, paging::PagedFile, util::{self, FileLike, NormalStringCompare, StringCompare, null_terminated_string}};

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
    pub(crate) open_files: Mutex<HashSet<FileRef>>,
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
    pub fn new_create(backing: impl FileLike + Send + Sync + 'static, conf: DriveConf<'_>) -> Result<Self, crate::fs::Error> {
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
        let mut dyndata = Dyndata::new(dyndata)?;
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

        Ok(Drive { s: Box::new(Shared { file: source, cft: Mutex::new(Cft::new(cft)), dyndata: Mutex::new(dyndata), path_cache: PathCache::new(), open_files: Mutex::new(HashSet::new()) }), header: Box::new(dr_header) })
    }

    pub fn new_existing(backing: impl FileLike + Send + Sync + 'static) -> Result<Self, crate::fs::Error> {
        let mut buf = [0u8; 256]; // first 8 bytes - paging info, then - drive header
        backing.read_exact_at(&mut buf, 0)?;
        let drive_header: &DriveHeader = bytemuck::from_bytes(&buf[8..256]);
        if drive_header.magic != *b"FARCDRV" || drive_header.version != 1 {
            return Err(FsError::InvalidDrive.into());
        }
        let page_size = 1u64 << drive_header.page_size;
        let source = PagedFile::new(backing, page_size);
        let cft = source.open_existing_block_cached(CENTRAL_FILE_DIRECTORY_BLOCK)?;
        let dyndata = source.open_existing_block_cached(DYNDATA_BLOCK)?;
        Ok(Drive { s: Box::new(Shared { file: source, cft: Mutex::new(Cft::new(cft)), dyndata: Mutex::new(Dyndata::new(dyndata)?), path_cache: PathCache::new(), open_files: Mutex::new(HashSet::new()) }), header: Box::new(*drive_header) })
    }

    /// Open a file for reading and writing.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn open<'a>(&'a self, file: impl ResolveToPath<'_>) -> Result<FileHandle<'a>, crate::fs::Error> {
        let file = file.resolve_to_existing(self)?;
        let mut cft = self.s.cft.lock().unwrap();        
        let file_record = 
            cft.read_file_record(file)?.ok_or(FsError::FileNotFound)?;
        
        assert!(file_record.flags.is_not_empty());
        assert!(file_record.uid == file.uid);
        if file_record.flags.is_dir() {
            return Err(FsError::IsADirectory.into());
        }
        self.lock_file(file)?;
        FileHandle::new(file_record, &self.s)
    }

    /// Create a new file and open it.
    /// 
    /// This method will create missing parent directories if they do not exist.
    /// It will fail if the file already exists.
    pub fn create_file<'a>(&'a self, path: impl ResolveToPath<'_>) -> Result<FileHandle<'a>, crate::fs::Error> {
        match path.resolve(self, true)? {
            sealed::ResolvedPath::Existing(_) => return Err(FsError::AlreadyExists.into()),
            sealed::ResolvedPath::New(parent_dir, file_name) => {
                let new_file_ref: FileRef = self.create_(parent_dir, file_name, false)?;
                self.open(new_file_ref)
            }
        }
    }

    pub fn create_directory(&self, path: impl ResolveToPath<'_>) -> Result<FileRef, crate::fs::Error> {
        match path.resolve(self, true)? {
            sealed::ResolvedPath::Existing(_) => return Err(FsError::AlreadyExists.into()),
            sealed::ResolvedPath::New(parent_dir, file_name) => {
                self.create_(parent_dir, file_name, true)
            }
        }
    }

    fn create_(&self, parent: FileRef, name: &[u8], is_directory: bool) -> Result<FileRef, crate::fs::Error> {
        let mut cft = self.s.cft.lock().unwrap();
        
        let mut parent_dir_record = cft.read_file_record(parent)?
            .ok_or(FsError::InvalidPath)?;
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
    pub fn info(&self, file: impl ResolveToPath<'_>) -> Result<Option<FileInfo>, crate::fs::Error> {
        let file = match file.resolve(self, false) {
            Ok(sealed::ResolvedPath::Existing(x)) => x,
            Ok(sealed::ResolvedPath::New(_, _)) => return Ok(None),
            Err(Error::FsError(FsError::FileNotFound | FsError::InvalidPath)) => return Ok(None),
            Err(e) => return Err(e),
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

    pub fn dir_entries(&self, dir: impl ResolveToPath<'_>) -> Result<Vec<FileRef>, crate::fs::Error> {
        let dir = dir.resolve_to_existing(self)?;
        let mut cft = self.s.cft.lock().unwrap();
        let dir_record = cft.read_file_record(dir)?
            .ok_or(FsError::FileNotFound)?;
        self.dir_entries_(&dir_record, usize::MAX)
    }

    fn dir_entries_(&self, directory: &FileRecord, limit: usize) -> Result<Vec<FileRef>, crate::fs::Error> {
        if !directory.flags.is_not_empty() {
            return Err(FsError::FileNotFound.into());
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
    /// Similarly to create, this operation will create missing parent directories if they do not exist.
    /// If a file already exists at `new_path`, this operation will fail unless `replace` is true, in which case the existing file will be deleted.
    pub fn move_(&self, path: impl ResolveToPath<'_>, new_path: impl ResolveToPath<'_>, replace: bool) -> Result<FileRef, crate::fs::Error> {
        let file = path.resolve_to_existing(self)?;
        let (new_parent_dir, new_name, replaced_file) = match new_path.resolve(self, true)? {
            sealed::ResolvedPath::New(x, y) => (x, y.into(), None),
            sealed::ResolvedPath::Existing(new_fr) =>
                if !replace {
                    return Err(FsError::AlreadyExists.into());
                } else {
                    self.lock_file(new_fr)?;
                    let fi = self.info(new_fr)?.unwrap();
                    (fi.parent, Cow::Owned(fi.name), Some(new_fr))
                }
        };
        let mut cft = self.s.cft.lock().unwrap();
        let mut file_record = cft.read_file_record(file)?.ok_or(FsError::FileNotFound)?;
        self.lock_file(file)?;
        
        let old_parent_dir = file_record.parent_uid;
        if old_parent_dir != new_parent_dir.uid {
            let old_parent_dir_record = cft.read_file_record(FileRef { uid: old_parent_dir })?.unwrap();
            debug_assert!(self.replace_file_in_directory_listing(&old_parent_dir_record, file, None)?);
            let mut new_parent_dir_record = cft.read_file_record(new_parent_dir)?
                .ok_or(FsError::FileNotFound)?;
            if let Some(replaced_file) = replaced_file {
                debug_assert!(self.replace_file_in_directory_listing(&new_parent_dir_record, replaced_file, Some(file))?);
            } else {
                self.add_file_to_directory_listing(&mut new_parent_dir_record, file)?;
            }
            cft.write_file_record(&new_parent_dir_record)?;
            file_record.parent_uid = new_parent_dir.uid;
        } else if let Some(replaced_file) = replaced_file {
            debug_assert!(self.replace_file_in_directory_listing(&cft.read_file_record(new_parent_dir)?.unwrap(), replaced_file, None)?);
        }
        self.s.path_cache.drop(old_parent_dir, file_record.get_filename(&self.s)?.as_ref());
        FileHandle::change_name(&mut file_record, &mut *self.s.dyndata.lock().unwrap(), &*new_name)?;
        self.s.open_files.lock().unwrap().remove(&file);
        cft.write_file_record(&file_record)?;
        if let Some(replaced_file) = replaced_file {
            // cleanup replaced file
            if let Some(file_record) = cft.read_file_record(replaced_file)? {
                if file_record.flags.has_longname() {
                    self.s.dyndata.lock().unwrap().try_update_slice(
                        u64::from_le_bytes(file_record.shortname[16..24].try_into().unwrap()), &[])?;
                }
                self.s.path_cache.drop(file_record.parent_uid, file_record.get_filename(&self.s)?.as_ref());
                cft.delete_file_record(replaced_file)?;
                self.s.file.mark_garbage(file_record.start_block)?;
                self.s.open_files.lock().unwrap().remove(&replaced_file);
            }
        }
        Ok(file)
    }

    /// Delete a file.
    /// This operation is exclusive and will fail if `file` is open.
    pub fn delete(&self, path: impl ResolveToPath<'_>) -> Result<(), crate::fs::Error> {
        let file = path.resolve_to_existing(self)?;
        let mut cft = self.s.cft.lock().unwrap();
        let file_record = cft.read_file_record(file)?.ok_or(FsError::FileNotFound)?;
        if file_record.flags.is_dir() {
            if !self.dir_entries_(&file_record, 1)?.is_empty() {
                return Err(FsError::DirectoryNotEmpty.into());
            }
        } else {
            self.lock_file(file)?;
        }
        if file_record.flags.has_longname() {
            self.s.dyndata.lock().unwrap().try_update_slice(
                u64::from_le_bytes(file_record.shortname[16..24].try_into().unwrap()), &[])?;
        }
        let parent_dir_record = cft.read_file_record(FileRef { uid: file_record.parent_uid })?.unwrap();
        debug_assert!(self.replace_file_in_directory_listing(&parent_dir_record, file, None)?);
        self.s.path_cache.drop(file_record.parent_uid, file_record.get_filename(&self.s)?.as_ref());
        self.s.open_files.lock().unwrap().remove(&file);
        // clear file_record
        cft.delete_file_record(file)?;
        self.s.file.mark_garbage(file_record.start_block)?;
        Ok(())
    }

    /// Alter the attributes of a file or directory.
    /// If the file is open, use [`FileHandle`] methods instead.
    pub fn alter(&self, path: impl ResolveToPath<'_>,
        readonly: Option<bool>, access_time: Option<u64>, 
        modify_time: Option<u64>, name: Option<&[u8]>) -> Result<(), crate::fs::Error> {
        let file = path.resolve_to_existing(self)?;
        let mut cft = self.s.cft.lock().unwrap();
        let mut file_record = cft.read_file_record(file)?.ok_or(FsError::FileNotFound)?;
        if self.s.open_files.lock().unwrap().contains(&file) {
            return Err(FsError::FileInUse.into());
        }
        if let Some(readonly) = readonly {
            file_record.flags.set_readonly(readonly);
        }
        if let Some(access_time) = access_time {
            file_record.access_time = access_time;
        }
        if let Some(modify_time) = modify_time {
            file_record.modify_time = modify_time;
        }
        if let Some(name) = name {
            self.s.path_cache.drop(file_record.parent_uid, file_record.get_filename(&self.s)?.as_ref());
            FileHandle::change_name(&mut file_record, &mut *self.s.dyndata.lock().unwrap(), name)?;
        }
        cft.write_file_record(&file_record)?;
        Ok(())
    }
        

    fn replace_file_in_directory_listing(&self, dir_record: &FileRecord, file: FileRef, new_file: Option<FileRef>) -> Result<bool, crate::fs::Error> {
        let new_bytes = new_file.map_or_else(|| [0; 8], |f| f.uid.to_le_bytes());
        if dir_record.flags.is_sdo() {
            if dir_record.start_block == 0 { return Ok(false); }
            let mut dd = self.s.dyndata.lock().unwrap();
            let mut listing = dd.get_slice(dir_record.start_block)?;
            debug_assert!(listing.len() == SDO_DIR_SIZE);
            for i in (0..SDO_DIR_SIZE).step_by(8) {
                if &listing[i..i+8] == &file.uid.to_le_bytes() {
                    listing[i..i+8].copy_from_slice(&new_bytes);
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
                    directory_listing.write_all(&new_bytes)?;
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    fn add_file_to_directory_listing(&self, dir_record: &mut FileRecord, file: FileRef) -> Result<(), crate::fs::Error> {
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
            directory_listing.write_all(&file.uid.to_le_bytes())?;
            Ok(())
        }
    }

    fn get_file_by_name<C: StringCompare>(&self, parent_dir: FileRef, filename: &[u8]) -> Result<Option<(FileRef, Option<FileInfo>)>, crate::fs::Error> {
        if let Some(cached_uid) = self.s.path_cache.lookup(parent_dir.uid, filename) {
            return Ok(Some((FileRef { uid: cached_uid }, None)));
        }
        let entries = self.dir_entries(parent_dir)?;
        for e in entries {
            if let Some(info) = self.info(e)? {
                if C::equal(&*info.name, filename) {
                    self.s.path_cache.store(parent_dir.uid, &*info.name, e.uid);
                    return Ok(Some((e, Some(info))));
                }
            }
        }
        Ok(None)
    }

    fn _resolve_path<'a, C: StringCompare>(&self, path: &'a [u8], create_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        let mut path_components = util::path_components(path);
        let file_name = match path_components.next_back() {
            None => return Ok(sealed::ResolvedPath::Existing(self.root())), // no path components = root dir
            Some(x) => x,
        };
        let mut dir = self.root();
        for component in path_components {
            match self.get_file_by_name::<C>(dir, component)? {
                Some((dref, dinfo)) => {
                    let dinfo = dinfo.map_or_else(|| self.info(dref), |i| Ok(Some(i)))?.unwrap();
                    if !dinfo.is_directory {
                        return Err(FsError::InvalidPath.into());
                    }
                    dir = dref;
                },
                None => {
                    if create_directories {
                        dir = self.create_(dir, component, true)?;
                    } else {
                        return Err(FsError::InvalidPath.into())
                    }
                },
            }
        };
        match self.get_file_by_name::<C>(dir, file_name)?.map(|x| x.0) {
            Some(f) => Ok(sealed::ResolvedPath::Existing(f)),
            None => Ok(sealed::ResolvedPath::New(dir, file_name)),
        }
    }

    pub fn resolve_path(&self, path: impl AsRef<[u8]>) -> Result<Option<FileRef>, crate::fs::Error> {
        match self._resolve_path::<NormalStringCompare>(&path.as_ref(), false) {
            Ok(sealed::ResolvedPath::Existing(f)) => Ok(Some(f)),
            Ok(sealed::ResolvedPath::New(_, _)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg(feature = "case-insensitive")]
    pub fn resolve_path_case_insensitive(&self, path: impl AsRef<[u8]>) -> Result<Option<FileRef>, crate::fs::Error> {
        use crate::util::CaseInsensitiveStringCompare;
        self._resolve_path::<CaseInsensitiveStringCompare>(&path.as_ref(), false, false)
    }

    pub fn drive_stats(&self) -> Result<DriveStats, crate::fs::Error> {
        let page_size = self.s.file.page_size();
        let used_pages = self.s.file.used_pages()?;
        let drive_name = null_terminated_string(&self.header.drive_name).to_vec().into_boxed_slice();
        let drive_owner = null_terminated_string(&self.header.drive_owner).to_vec().into_boxed_slice();
        Ok(DriveStats { page_size, used_pages, drive_name, drive_owner })
    }

    pub fn flush(&self) -> Result<(), crate::fs::Error> {
        self.s.cft.lock().unwrap().flush()?;
        self.s.dyndata.lock().unwrap().flush()?;
        self.s.file.flush()?;
        Ok(())
    }

    fn lock_file(&self, file: FileRef) -> Result<(), crate::fs::Error> {
        if !self.s.open_files.lock().unwrap().insert(file) {
            return Err(FsError::FileInUse.into());
        }
        Ok(())
    }
}

pub struct DriveStats {
    pub page_size: u64,
    pub used_pages: u64,
    pub drive_name: Box<[u8]>,
    pub drive_owner: Box<[u8]>,
}

mod sealed {
    pub enum ResolvedPath<'a> {
        Existing(super::FileRef),
        /// (parent_directory, child_name)
        New(super::FileRef, &'a [u8]),
    }
    pub trait ResolveToPathSealed<'a> : 'a where Self: Sized {
        /// Resolve the path to either an existing file or a new file (with its parent directory and name).
        /// 
        /// If `create_missing_directories` is true, missing parent directories will be created.
        /// If it is false and a directory in the path does not exist, [`FsError::InvalidPath`] will be returned.
        fn resolve(self, drive: &super::Drive, create_missing_directories: bool) -> Result<ResolvedPath<'a>, crate::fs::Error>;

        fn resolve_to_existing(self, drive: &super::Drive) -> Result<super::FileRef, crate::fs::Error> {
            match self.resolve(drive, false)? {
                ResolvedPath::Existing(f) => Ok(f),
                ResolvedPath::New(_, _) => Err(crate::fs::FsError::FileNotFound.into()),
            }
        }
    }
}

impl<'a> sealed::ResolveToPathSealed<'a> for &'a [u8] {
    fn resolve(self, drive: &super::Drive, create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        drive._resolve_path::<NormalStringCompare>(self, create_missing_directories)
    }
}
impl<'a> ResolveToPath<'a> for &'a [u8] {}

impl<'a> sealed::ResolveToPathSealed<'a> for &'a str {
    fn resolve(self, drive: &super::Drive, create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        drive._resolve_path::<NormalStringCompare>(self.as_bytes(), create_missing_directories)
    }
}
impl<'a> ResolveToPath<'a> for &'a str {}

impl<'a, const N: usize> sealed::ResolveToPathSealed<'a> for &'a [u8; N] {
    fn resolve(self, drive: &super::Drive, create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        self.as_slice().resolve(drive, create_missing_directories)
    }
}
impl<'a, const N: usize> ResolveToPath<'a> for &'a [u8; N] {}

impl<'a> sealed::ResolveToPathSealed<'a> for FileRef {
    fn resolve(self, _drive: &super::Drive, _create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        Ok(sealed::ResolvedPath::Existing(self))
    }
}
impl<'a> ResolveToPath<'a> for FileRef {}

impl<'a> sealed::ResolveToPathSealed<'a> for &'a FileHandle<'_> {
    fn resolve(self, _drive: &super::Drive, _create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        Ok(sealed::ResolvedPath::Existing(self.get_ref()))
    }
}
impl<'a> ResolveToPath<'a> for &'a FileHandle<'_> {}

impl<'a> sealed::ResolveToPathSealed<'a> for (FileRef, &'a [u8]) {
    fn resolve(self, drive: &super::Drive, _create_missing_directories: bool) -> Result<sealed::ResolvedPath<'a>, crate::fs::Error> {
        match drive.get_file_by_name::<NormalStringCompare>(self.0, self.1) {
            Ok(Some((file, _))) => Ok(sealed::ResolvedPath::Existing(file)),
            Ok(None) => Ok(sealed::ResolvedPath::New(self.0, self.1)),
            Err(e) => Err(e),
        }
    }
}
impl<'a> ResolveToPath<'a> for (FileRef, &'a [u8]) {}

/// Types which may be used to reference a file/directory in the drive.
/// Implemented by `&[u8]` and `str` (representing the path to the file),
/// `(FileRef, &[u8])` (representing an existing parent directory and the file's name),
/// `FileRef` (representing the file itself), and `&FileHandle` (representing an open file).
pub trait ResolveToPath<'a> : sealed::ResolveToPathSealed<'a> {}

const SDO_DIR_SIZE: usize = 40; // 5 files, 8 bytes each

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
/// A non-owning reference to a file. The file may or may not exist.
pub struct FileRef { pub(crate) uid: u64 }

impl From<FileRef> for u64 {
    fn from(f: FileRef) -> Self { f.uid }
}
impl From<u64> for FileRef {
    fn from(uid: u64) -> Self { FileRef { uid } }
}
