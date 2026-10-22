use std::{ffi::OsStr, io::{Read, Seek, Write}, os::{unix::ffi::OsStrExt}, sync::{Mutex, atomic::{AtomicBool, AtomicU64}}, time::{Duration, SystemTime}};

use farfs::fs::{FileHandle, FileInfo, FileRef, FsError};
use fuser::{Errno, FileAttr, Filesystem, OpenAccMode};
use log::{trace, error};

struct OpenFile {
    h: Mutex<FileHandle<'static>>,
    delete_on_close: AtomicBool,
}

pub struct Fs {
    drive: Box<farfs::fs::Drive>, // make sure Drive is not moved or dropped
    open_files: lockfree::map::Map<FileRef, std::sync::Weak<OpenFile>>,
    fhs: lockfree::map::Map<u64, std::sync::Arc<OpenFile>>,
    fh_counter: AtomicU64,
    // the uid and gid of the user who owns the filesystem
    owner_uid: u32,
    owner_gid: u32,
}

const TTL: Duration = Duration::from_secs(60);

macro_rules! unwrap {
    ($val: expr, $reply: expr) => {
        match $val {
            Ok(x) => x,
            Err(farfs::fs::Error::FsError(e)) => {
                error!("Error: {:?}", e);
                $reply.error(Errno::from(e.into_io_error().kind()));
                return;
            }
            Err(farfs::fs::Error::BackingError(e)) => {
                error!("Error: {:?}", e);
                $reply.error(Errno::from(e.kind()));
                return;
            }
        }
    };
}

// INode cannot be zero
fn get_fr(ino: fuser::INodeNo) -> FileRef {
    FileRef::from(ino.0 - 1)
}

fn inode(fr: FileRef) -> fuser::INodeNo {
    fuser::INodeNo(Into::<u64>::into(fr) + 1)
}

impl Filesystem for Fs {
    fn init(&mut self, _req: &fuser::Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        trace!("init()");
        let _ = config.set_time_granularity(Duration::from_secs(1));
        Ok(())
    }

    fn getattr(&self, _req: &fuser::Request, ino: fuser::INodeNo, fh: Option<fuser::FileHandle>, reply: fuser::ReplyAttr) {
        let fr = get_fr(ino);
        trace!("getattr({:?})", fr);
        let finfo = 
            if let Some(fh) = fh {
                unwrap!(self.with_handle(fh, |h| {
                    h.full_fileinfo()
                }), reply)
            } else if let Some(x) = unwrap!(self.drive.info(fr), reply) {
                x
            } else { return reply.error(Errno::ENOENT); };
        let attr = self.fileattr_from_fileinfo(&finfo);
        reply.attr(&TTL, &attr); 
    }

    fn lookup(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &std::ffi::OsStr, reply: fuser::ReplyEntry) {
        let parent_fr = get_fr(parent);
        trace!("lookup({:?}, {:?})", parent_fr, name);
        match unwrap!(self.drive.info((parent_fr, name.as_encoded_bytes())), reply) {
            Some(finfo) => {
                let attr = self.fileattr_from_fileinfo(&finfo);
                reply.entry(&TTL, &attr, fuser::Generation(0));
            },
            None => {
                trace!("file not found");
                reply.error(Errno::ENOENT)
            }
        }
    }

    fn readdir(&self, _req: &fuser::Request, ino: fuser::INodeNo, _fh: fuser::FileHandle, offset: u64, mut reply: fuser::ReplyDirectory) {
        let fr = get_fr(ino);
        trace!("readdir({:?}, {:?})", fr, offset);
        for (i, child_fr) in unwrap!(self.drive.dir_entries(fr), reply).iter().enumerate() {
            if i < offset as usize { continue; }
            let finfo = match unwrap!(self.drive.info(*child_fr), reply) {
                Some(f) => f,
                None => continue,
            };
            let full = reply.add(
                inode(*child_fr), 
                (i + 1) as _,
                if finfo.is_directory { fuser::FileType::Directory } else { fuser::FileType::RegularFile },
                OsStr::from_bytes(&finfo.name));
            if full { break; }
        }
        reply.ok();
    }

    fn open(&self, _req: &fuser::Request, ino: fuser::INodeNo, flags: fuser::OpenFlags, reply: fuser::ReplyOpen) {
        let fr = get_fr(ino);
        trace!("open({:?}, {:?})", fr, flags);
        let fh = unwrap!(self.new_fh(fr), reply);
        let handle = self.fhs.get(&fh).unwrap();
        if handle.1.h.lock().unwrap().is_readonly() && flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EACCES);
            return;
        }
        std::mem::drop(handle);
        reply.opened(fuser::FileHandle(fh), fuser::FopenFlags::empty());
    }

    fn release(&self, _req: &fuser::Request, ino: fuser::INodeNo, fh: fuser::FileHandle, _flags: fuser::OpenFlags, _lock_owner: Option<fuser::LockOwner>, _flush: bool, reply: fuser::ReplyEmpty) {
        let fr = get_fr(ino);
        trace!("release({:?})", fr);
        if let Some(guard) = self.fhs.remove(&fh.0) {
            let openfile = spin_until_removed(guard);
            if let Some(openfile) = std::sync::Arc::into_inner(openfile) {
                // only run this code if this is the last reference to the open file
                let _ = openfile.h.into_inner().unwrap().close();
                self.open_files.remove(&fr);
                let delete_on_close = openfile.delete_on_close.load(std::sync::atomic::Ordering::SeqCst);
                if delete_on_close {
                    std::mem::drop(self.drive.delete(fr));
                }
            }
        }
        reply.ok();
    }

    fn create(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &std::ffi::OsStr, mode: u32, _umask: u32, flags: i32, reply: fuser::ReplyCreate) {
        let parent_fr = get_fr(parent);
        trace!("create({:?}, {:?}, {:o}, {:x})", parent_fr, name, mode, flags);
        let handle = match self.drive.create_file((parent_fr, name.as_encoded_bytes())) {
            Ok(x) => x,
            Err(farfs::fs::Error::FsError(FsError::AlreadyExists)) => {
                if flags & libc::O_EXCL != 0 {
                    reply.error(Errno::EEXIST);
                    return;
                }
                unwrap!(self.drive.open((parent_fr, name.as_encoded_bytes())), reply)
            },
            Err(e) => unwrap!(Err(e), reply),
        };
        let mut handle = unsafe { std::mem::transmute::<FileHandle<'_>, FileHandle<'static>>(handle) };
        if flags & libc::O_TRUNC != 0 {
            unwrap!(handle.set_len(0), reply);
        }
        let finfo = unwrap!(handle.full_fileinfo(), reply);
        if finfo.is_readonly && flags & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(Errno::EACCES);
            return;
        }
        let attr = self.fileattr_from_fileinfo(&finfo);
        let openfile = std::sync::Arc::new(OpenFile { h: Mutex::new(handle), delete_on_close: AtomicBool::new(false) });
        self.open_files.insert(finfo.self_ref, std::sync::Arc::downgrade(&openfile));
        let fh = self.fh_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.fhs.insert(fh, openfile);
        reply.created(&TTL, &attr, fuser::Generation(0), fuser::FileHandle(fh), fuser::FopenFlags::empty());
    }

    fn mkdir(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &std::ffi::OsStr, mode: u32, _umask: u32, reply: fuser::ReplyEntry) {
        let parent_fr = get_fr(parent);
        trace!("mkdir({:?}, {:?}, {:o})", parent_fr, name, mode);
        let fr = unwrap!(self.drive.create_directory((parent_fr, name.as_encoded_bytes())), reply);
        let finfo = unwrap!(self.drive.info(fr), reply).unwrap();
        let attr = self.fileattr_from_fileinfo(&finfo);
        reply.entry(&TTL, &attr, fuser::Generation(0));
    }

    fn setattr(&self, _req: &fuser::Request,
        ino: fuser::INodeNo, mode: Option<u32>, _: Option<u32>, _: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>, _fh: Option<fuser::FileHandle>, _: Option<SystemTime>, _: Option<SystemTime>, _: Option<SystemTime>, _: Option<fuser::BsdFileFlags>,
        reply: fuser::ReplyAttr)
    {
        fn time_or_now(t: fuser::TimeOrNow) -> u64 {
            match t {
                fuser::TimeOrNow::SpecificTime(st) => st,
                fuser::TimeOrNow::Now => SystemTime::now(),
            }.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
        }

        let fr = get_fr(ino);
        trace!("setattr({:?}, mode={:?}, size={:?}, atime={:?}, mtime={:?})", fr, mode, size, atime, mtime);
        // we interpret mode_t into our readonly flag
        let readonly = mode.map(|m| m & 0o222 == 0);
        // use fr and open_files instead of fh, because fh may not always be passed to us
        let finfo;
        if let Some(guard) = self.open_files.get(&fr) &&
           let Some(openfile) = guard.1.upgrade() {
            let mut handle = openfile.h.lock().unwrap();
            if let Some(readonly) = readonly { handle.set_readonly(readonly); }
            if let Some(size) = size { unwrap!(handle.set_len(size), reply); }
            if let Some(atime) = atime { handle.set_access_time(time_or_now(atime)); }
            if let Some(mtime) = mtime { handle.set_modify_time(time_or_now(mtime)); }
            let _ = handle.flush();
            finfo = unwrap!(handle.full_fileinfo(), reply);
        } else {
            // file is not open, fall back to using alter
            unwrap!(self.drive.alter(fr, readonly,
                atime.map(time_or_now), mtime.map(time_or_now), None), reply);
            // alter doesn't change file size, we have to do it manually if it's required
            if let Some(size) = size {
                let mut handle = unwrap!(self.drive.open(fr), reply);
                unwrap!(handle.set_len(size), reply);
                unwrap!(handle.close().map_err(farfs::fs::Error::BackingError), reply);
            }
            finfo = unwrap!(self.drive.info(fr), reply).unwrap()
        }
        let attr = self.fileattr_from_fileinfo(&finfo);
        reply.attr(&TTL, &attr);
    }

    fn unlink(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &std::ffi::OsStr, reply: fuser::ReplyEmpty) {
        let parent_fr = get_fr(parent);
        trace!("unlink({:?}, {:?})", parent_fr, name);
        match self.drive.delete((parent_fr, name.as_encoded_bytes())) {
            Ok(()) => { trace!("deleted directly"); reply.ok() },
            Err(farfs::fs::Error::FsError(FsError::FileInUse)) => {
                // file is currently open
                let fr = unwrap!(self.drive.info((parent_fr, name.as_encoded_bytes())), reply).unwrap().self_ref;
                self.open_files.get(&fr).map(|guard| {
                    guard.1.upgrade().unwrap()
                        .delete_on_close.store(true, std::sync::atomic::Ordering::SeqCst);
                    // mark for deletion when the file is closed
                });
                trace!("marked for deletion on close");
                reply.ok()
            }
            Err(e) => unwrap!(Err(e), reply),
        }
    }

    fn rmdir(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        let parent_fr = get_fr(parent);
        trace!("rmdir({:?}, {:?})", parent_fr, name);
        match self.drive.delete((parent_fr, name.as_encoded_bytes())) {
            Ok(()) => reply.ok(),
            Err(e) => unwrap!(Err(e), reply),
        }
    }

    fn flush(&self, _req: &fuser::Request, ino: fuser::INodeNo, _fh: fuser::FileHandle, _lock_owner: fuser::LockOwner, reply: fuser::ReplyEmpty) {
       self.fsync(_req, ino, _fh, false, reply);
    }

    fn read(&self, _req: &fuser::Request, ino: fuser::INodeNo, fh: fuser::FileHandle, offset: u64, size: u32, flags: fuser::OpenFlags, _lock_owner: Option<fuser::LockOwner>, reply: fuser::ReplyData) {
        let fr = get_fr(ino);
        trace!("read({:?}, offset={}, size={}, flags={:?})", fr, offset, size, flags);
        let mut buf = vec![0u8; size as usize];
        let n = unwrap!(self.with_handle(fh, |handle| {
            handle.seek(std::io::SeekFrom::Start(offset))?;
            Ok(handle.read(&mut buf)?)
        }), reply);
        reply.data(&buf[..n]);
    }

    fn write(&self, _req: &fuser::Request, ino: fuser::INodeNo, fh: fuser::FileHandle, offset: u64, data: &[u8], write_flags: fuser::WriteFlags, flags: fuser::OpenFlags, _lock_owner: Option<fuser::LockOwner>, reply: fuser::ReplyWrite)
    {
        let fr = get_fr(ino);
        trace!("write({:?}, offset={}, size={}, write_flags={:?}, flags={:?})", fr, offset, data.len(), write_flags, flags);
        let n = unwrap!(self.with_handle(fh, |handle| {
            if flags.acc_mode() == OpenAccMode::O_RDONLY {
                return Err(farfs::fs::Error::FsError(FsError::PermissionDenied));
            }
            if flags.0 & libc::O_APPEND != 0 {
                handle.seek(std::io::SeekFrom::End(0))?;
            } else {
                handle.seek(std::io::SeekFrom::Start(offset))?;
            }
            Ok(handle.write(data)?)
        }), reply);
        reply.written(n as u32);
    }

    fn rename(&self, _req: &fuser::Request, parent: fuser::INodeNo, name: &OsStr, newparent: fuser::INodeNo, newname: &OsStr, flags: fuser::RenameFlags, reply: fuser::ReplyEmpty) {
        trace!("rename({:?}, {:?}, {:?}, {:?}, flags={:?})", get_fr(parent), name, get_fr(newparent), newname, flags);
        let fr = match unwrap!(self.drive.info((get_fr(parent), name.as_encoded_bytes())), reply) {
            Some(x) => x.self_ref,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let replace = !flags.contains(fuser::RenameFlags::RENAME_NOREPLACE);
        match self.drive.move_(fr, (get_fr(newparent), newname.as_encoded_bytes()), replace) {
            Ok(_) => reply.ok(),
            Err(farfs::fs::Error::FsError(FsError::FileInUse)) => {
                reply.error(Errno::EAGAIN);
                // TODO
            }
            Err(e) => unwrap!(Err(e), reply),
        }
    }

    fn ioctl(&self, _req: &fuser::Request, ino: fuser::INodeNo, _fh: fuser::FileHandle, flags: fuser::IoctlFlags, cmd: u32, _in_data: &[u8], _out_size: u32, reply: fuser::ReplyIoctl) {
        let fr = get_fr(ino);
        trace!("ioctl({:?}, flags={:?}, cmd={:#x}", fr, flags, cmd);
        match unwrap!(self.drive.info(fr), reply) {
            None => reply.error(Errno::ENOENT),
            Some(_) => {
                if cmd & 0xFF00 == 0x5400 {
                    // tty ioctl, we don't support terminals
                    reply.error(Errno::ENOTTY);
                } else {
                    reply.error(Errno::ENOSYS);
                }
            }
        }
    }

    fn fsync(&self, _req: &fuser::Request, ino: fuser::INodeNo, _fh: fuser::FileHandle, _datasync: bool, reply: fuser::ReplyEmpty) {
        let fr = get_fr(ino);
        trace!("flush/fsync({:?})", fr);
        match self.open_files.get(&fr) {
            Some(guard) => {
                if let Some(openfile) = guard.1.upgrade() {
                    let _ = openfile.h.lock().unwrap().flush();
                }
            },
            None => {}
        }
        reply.ok();
    }
}

impl Fs {
    fn new_fh(&self, fr: FileRef) -> Result<u64, farfs::fs::Error> {
        let openfile = 
            if let Some(weak) = self.open_files.get(&fr)
                && let Some(x) = weak.1.upgrade() {
                        x
            } else {
                let handle = self.drive.open(fr)?;
                let handle = unsafe { std::mem::transmute::<FileHandle<'_>, FileHandle<'static>>(handle) };
                let openfile = std::sync::Arc::new(OpenFile { h: Mutex::new(handle), delete_on_close: AtomicBool::new(false) });
                self.open_files.insert(fr, std::sync::Arc::downgrade(&openfile));
                openfile
            };
        let fh = self.fh_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.fhs.insert(fh, openfile);
        Ok(fh)
    }

    fn with_handle<T>(&self, fh: fuser::FileHandle, f: impl FnOnce(&mut FileHandle<'static>) -> Result<T, farfs::fs::Error>) -> Result<T, farfs::fs::Error> {
        self.fhs.get(&fh.0)
            .ok_or(farfs::fs::Error::FsError(FsError::FileNotOpen))
            .and_then(|guard| {
                let mut handle = guard.1.h.lock().unwrap();
                f(&mut handle)
            })
    }

    fn fileattr_from_fileinfo(&self, info: &FileInfo) -> FileAttr {
        let crtime = SystemTime::UNIX_EPOCH + Duration::from_secs(info.create_time);
        FileAttr { 
            ino: inode(info.self_ref),
            size: info.size, 
            blocks: info.size.div_ceil(512), 
            // directories don't track access and modify times
            atime: info.access_time.map_or(crtime, |t| SystemTime::UNIX_EPOCH + Duration::from_secs(t)), 
            mtime: info.modify_time.map_or(crtime, |t| SystemTime::UNIX_EPOCH + Duration::from_secs(t)),
            ctime: info.modify_time.map_or(crtime, |t| SystemTime::UNIX_EPOCH + Duration::from_secs(t)),
            crtime: SystemTime::UNIX_EPOCH + Duration::from_secs(info.create_time),
            kind: if info.is_directory { fuser::FileType::Directory } else { fuser::FileType::RegularFile}, 
            perm: if info.is_readonly { 0o444 } else if info.is_directory { 0o755 } else { 0o644 },
            nlink: if info.is_directory { 2 } else { 1 }, 
            uid: self.owner_uid,
            gid: self.owner_gid, 
            rdev: 0, 
            blksize: 4096, 
            flags: 0,
        }
    }
}

pub(crate) fn spin_until_removed<K, V>(mut x: lockfree::map::Removed<K, V>) -> V {
    loop {
        match lockfree::map::Removed::try_into(x) {
            Ok((_, v)) => return v,
            Err(r) => {
                x = r;
                std::thread::yield_now();
            }
        }
    }
}

/// Mount a filesystem.
/// owner_uid and owner_gid are the permissions implicitly given to all files on the drive
/// `user_allow_other` can be set to true if the user has set `user_allow_other` in `/etc/fuse.conf`
pub fn mount(
    drive: farfs::fs::Drive,
    mount_point: impl AsRef<std::path::Path>,
    owner_uid: u32,
    owner_gid: u32,
    logger: Option<impl Write + Send + 'static>,
    loglevel: log::LevelFilter,
    fs_name: Option<&str>,
    user_allow_other: bool) -> std::io::Result<()> {
        if let Some(logger) = logger {
            simplelog::WriteLogger::init(loglevel, simplelog::Config::default(), logger).unwrap();
        }
        let fs = Fs { drive: Box::new(drive), owner_uid, owner_gid, open_files: lockfree::map::Map::new(), fhs: lockfree::map::Map::new(), fh_counter: AtomicU64::new(3) };
        let mut conf = fuser::Config::default();
        conf.mount_options = vec![
            fuser::MountOption::RW,
            fuser::MountOption::Exec,
            fuser::MountOption::Sync,
            fuser::MountOption::DefaultPermissions,
        ];
        if let Some(fs_name) = fs_name {
            conf.mount_options.push(fuser::MountOption::FSName(fs_name.to_string()));
        }
        conf.acl = if user_allow_other {
            conf.mount_options.push(fuser::MountOption::AutoUnmount);
            fuser::SessionACL::All
        } else { fuser::SessionACL::Owner };
        conf.n_threads = Some(1);
        fuser::mount(fs, mount_point, &conf)
}