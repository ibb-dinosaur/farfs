use std::{collections::{HashMap, HashSet}, ffi::{CStr, CString}, fs::File, io::ErrorKind::{NotFound, ResourceBusy}, ops::DerefMut, sync::Mutex};

use filearchive2::{drive::{Drive, FileHandle, FileInfo, FileRef, DriveConf}};

use libc::{c_char, c_int, c_void};
#[cfg(unix)]
mod sys_imports {
    pub use libc::{mode_t, uid_t, gid_t, stat, timespec, off_t};
    pub use libfuse_sys::fuse::{fuse_get_context, fuse_main, fuse_operations, fuse_file_info, fuse_fill_dir_t, fuse_readdir_flags, fuse_conn_info, fuse_config};
}
#[cfg(windows)]
mod sys_imports {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    include!(concat!(env!("OUT_DIR"), "/fuse_bindings.rs"));
    pub type mode_t = u32;
    pub type uid_t = u32;
    pub type gid_t = u32;
    pub type stat = fuse_stat;
    pub type timespec = fuse_timespec;
    pub type off_t = i64;
    unsafe extern "C" {
        #[link_name = "fuse_main_real__extern"]
        pub fn fuse_main_real(argc: libc::c_int, argv: *mut *mut libc::c_char, ops: *const fuse_operations, opsize: usize, data: *mut libc::c_void,) -> libc::c_int;
        #[link_name = "fuse_get_context__extern"]
        pub fn fuse_get_context() -> *mut fuse_context;
    }
}

use sys_imports::*;
use std::io::{Read, Write, Seek};

fn main() {
    let mut argv = std::env::args().map(|arg| CString::new(arg).unwrap().into_raw()).collect::<Vec<_>>();
    unsafe {
        #[cfg(unix)]
        fuse_main(argv.len() as _, argv.as_mut_ptr(), &FUSE, std::ptr::null_mut());
        #[cfg(windows)]
        fuse_main_real(argv.len() as _, argv.as_mut_ptr(), &FUSE, std::mem::size_of::<fuse_operations>(), std::ptr::null_mut());
    }
}

struct PrivateData {
    drive: Drive,
    // sometimes, we get requests to do something with a file
    // that's technically open without getting a file handle.
    // in this case, use this to look it up.
    live_handles: Mutex<HashMap<FileRef, *const Mutex<FileHandle<'static>>>>,
    to_be_deleted: Mutex<HashSet<FileRef>>,
}

fn get_drive() -> &'static Drive {
    unsafe { &(*((*fuse_get_context()).private_data as *const PrivateData)).drive }
}

fn get_live_handles() -> &'static Mutex<HashMap<FileRef, *const Mutex<FileHandle<'static>>>> {
    unsafe { &(*((*fuse_get_context()).private_data as *const PrivateData)).live_handles }
}

fn get_to_be_deleted() -> &'static Mutex<HashSet<FileRef>> {
    unsafe { &(*((*fuse_get_context()).private_data as *const PrivateData)).to_be_deleted }
}

macro_rules! unwrap {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => { 
                eprintln!("Error: {}", e);
                return e.raw_os_error().unwrap_or(-1) as _; 
            }
        }
    };
}

fn fill_stat_from_fileinfo(st: &mut stat, fileinfo: &FileInfo) {
    if fileinfo.is_directory {
        st.st_mode = (libc::S_IFDIR | 0o0755) as u32;
        st.st_nlink = 2;
    } else if fileinfo.is_readonly {
        st.st_mode = (libc::S_IFREG | 0o0444) as u32;
        st.st_nlink = 1;
    } else {
        st.st_mode = (libc::S_IFREG | 0o0644) as u32;
        st.st_nlink = 1;
    }
    st.st_uid = unsafe { (*fuse_get_context()).uid };
    st.st_gid = unsafe { (*fuse_get_context()).gid };
    st.st_size = fileinfo.size as _;
    #[cfg(unix)]
    {
        st.st_atime = fileinfo.access_time.unwrap_or(fileinfo.create_time) as _;
        st.st_mtime = fileinfo.modify_time.unwrap_or(fileinfo.create_time) as _;
        st.st_ctime = st.st_mtime;
    }
    #[cfg(windows)]
    {
        st.st_atim.tv_sec = fileinfo.access_time.unwrap_or(fileinfo.create_time) as _;
        st.st_mtim.tv_sec = fileinfo.modify_time.unwrap_or(fileinfo.create_time) as _;
        st.st_ctim.tv_sec = st.st_mtim.tv_sec;
    }
}

// Gets file handle:
// 1. from fi if it's not null
// 2. from live_handles if file is open
// 3. by opening the file if it's not open
unsafe fn with_file_handle(path: *const c_char, fi: *mut fuse_file_info, f: impl FnOnce(&mut FileHandle) -> c_int) -> c_int {
    if !fi.is_null() {
        let handle = (*fi).fh as *const Mutex<FileHandle>;
        f(&mut (*handle).lock().unwrap())
    } else {
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        let file_ref = match unwrap!(drive.resolve_path(path)) {
            None => return -libc::ENOENT,
            Some(x) => x
        };
        match get_live_handles().lock().unwrap().get(&file_ref) {
            Some(&ptr) => f(&mut (*ptr).lock().unwrap()),
            None => { // try to open file
                let mut handle = match unwrap!(drive.open(file_ref)) {
                    Ok(x) => x,
                    Err(e) if e.kind() == NotFound => return -libc::ENOENT,
                    Err(_) => return -libc::EIO,
                };
                let result = f(&mut handle);
                // don't forget to close the file handle
                std::mem::drop(handle);
                result
            }
        }
    }   
}

unsafe extern "C" fn fuse_getattr(path: *const c_char, stat: *mut stat, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_getattr({:?})", CStr::from_ptr(path));
        (*stat) = std::mem::zeroed();
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        let fileinfo = unwrap!(drive.info(path));
        match fileinfo {
            None => -libc::ENOENT,
            Some(fileinfo) => {
                fill_stat_from_fileinfo(&mut *stat, &fileinfo);
                0
            }
        }
    }
}

// fn fill(buf: *mut c_void, name: *const i8, stbuf: *const stat, off: i64, flags: u32) -> i32
unsafe extern "C" fn fuse_readdir(path: *const c_char, buf: *mut c_void, fill: fuse_fill_dir_t, off: off_t, fi: *mut fuse_file_info, flags: fuse_readdir_flags) -> c_int {
    unsafe {
        eprintln!("fuse_readdir({:?})", CStr::from_ptr(path));
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        let fill = fill.unwrap();

        let entries = match drive.dir_entries(path) {
            Ok(x) => x,
            Err(e) if e.kind() == NotFound => return -libc::ENOENT,
            Err(_) => return -libc::EIO,
        };
        
        (fill)(buf, CString::new(".").unwrap().as_ptr(), std::ptr::null(), 0, 0);
        (fill)(buf, CString::new("..").unwrap().as_ptr(), std::ptr::null(), 0, 0);
        for entry in entries {
            let entry_info = unwrap!(drive.info(entry));
            match entry_info {
                None => return -libc::ENOENT,
                Some(mut entry_info) => {
                    let name = CString::new(std::mem::take(&mut entry_info.name)).unwrap();
                    let mut st: stat = std::mem::zeroed();
                    fill_stat_from_fileinfo(&mut st, &entry_info);
                    (fill)(buf, name.as_ptr(), &st, 0, 0);
                }
            }
        }
        0
    }
}

unsafe extern "C" fn fuse_init(conn: *mut fuse_conn_info, cfg: *mut fuse_config) -> *mut c_void {
    unsafe {
        (*conn).want = 0;
        (*conn).time_gran = 1000000000; // second resolution
    }
    let path = "backing_archive.drive";
    let drive = if std::path::Path::new(path).exists() {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        Drive::new_existing(file).unwrap()
    } else {
        let file = std::fs::File::create_new(path).unwrap();
        Drive::new_create(file, DriveConf::default()).unwrap()
    };
    let priv_data = PrivateData { drive, live_handles: Mutex::new(HashMap::new()), to_be_deleted: Mutex::new(HashSet::new()) };
    Box::into_raw(Box::new(priv_data)) as _
}

unsafe extern "C" fn fuse_destroy(private_data: *mut c_void) {
    unsafe {
        std::mem::drop(Box::from_raw(private_data as *mut PrivateData));
    }
}

unsafe extern "C" fn fuse_open(path: *const c_char, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_open({:?})", CStr::from_ptr(path));
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        let handle = match drive.open(path) {
            Ok(x) => x,
            Err(e) if e.kind() == NotFound => return -libc::ENOENT,
            Err(_) => return -libc::EIO,
        };
        let file_ref = handle.get_ref();
        (*fi).fh = Box::into_raw(Box::new(Mutex::new(handle))) as _;
        get_live_handles().lock().unwrap().insert(file_ref, (*fi).fh as *const Mutex<FileHandle>);
        0
    }
}

unsafe extern "C" fn fuse_release(path: *const c_char, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_release({:?})", CStr::from_ptr(path));
        let handle = Box::from_raw((*fi).fh as *mut Mutex<FileHandle>);
        let file_ref = handle.lock().unwrap().get_ref();
        get_live_handles().lock().unwrap().remove(&file_ref);
        if get_to_be_deleted().lock().unwrap().remove(&file_ref) {
            // the file was marked for deletion while it was open. delete it now.
            let drive = get_drive();
            let _ = drive.delete(file_ref);
        }
        std::mem::drop(handle);
        0
    }
}

unsafe extern "C" fn fuse_create(path: *const c_char, mode: mode_t, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_create({:?})", CStr::from_ptr(path));
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        let handle = unwrap!(drive.create_file(path));
        let file_ref = handle.get_ref();
        (*fi).fh = Box::into_raw(Box::new(Mutex::new(handle))) as _;
        get_live_handles().lock().unwrap().insert(file_ref, (*fi).fh as *const Mutex<FileHandle>);
        0
    }
}

unsafe extern "C" fn fuse_mkdir(path: *const c_char, mode: mode_t) -> c_int {
    unsafe {
        eprintln!("fuse_mkdir({:?})", CStr::from_ptr(path));
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        unwrap!(drive.create_directory(path));
        0
    }
}

unsafe extern "C" fn fuse_utimens(path: *const c_char, tv: *const timespec, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_utimens({:?})", CStr::from_ptr(path));
        with_file_handle(path, fi, |handle| {
            handle.set_access_time((*tv.offset(0)).tv_sec as _);
            handle.set_modify_time((*tv.offset(1)).tv_sec as _);
            0
        })
    }
}

unsafe extern "C" fn fuse_unlink(path: *const c_char) -> c_int {
    unsafe {
        eprintln!("fuse_unlink({:?})", CStr::from_ptr(path));
        let drive = get_drive();
        let path = CStr::from_ptr(path).to_bytes();
        match drive.delete(path) {
            Ok(_) => 0,
            Err(e) if e.kind() == NotFound => -libc::ENOENT,
            Err(e) if e.kind() == ResourceBusy => {
                // the file is currently open. we can't delete it, but we can mark it for deletion when it's closed.
                let file_ref = unwrap!(drive.resolve_path(path)).unwrap();
                get_to_be_deleted().lock().unwrap().insert(file_ref);
                0
            }
            Err(e) => { eprintln!("{:?}", e); -libc::EIO },
        }
    }
}

unsafe extern "C" fn fuse_rename(oldpath: *const c_char, newpath: *const c_char, flags: u32) -> c_int {
    unsafe {
        // TODO: handle flags
        eprintln!("fuse_rename({:?}, {:?})", CStr::from_ptr(oldpath), CStr::from_ptr(newpath));
        let drive = get_drive();
        let oldpath = CStr::from_ptr(oldpath).to_bytes();
        let newpath = CStr::from_ptr(newpath).to_bytes();
        match drive.move_(oldpath, newpath) {
            Ok(_) => 0,
            Err(e) if e.kind() == NotFound => -libc::ENOENT,
            Err(e) if e.kind() == ResourceBusy => {
                // the file is currently open. sometimes the fuse client will try to rename an open file,
                // which we don't allow. to get around it, we close the file handle, rename it, and then re-open it. this is a bit of a hack, but it works.
                let file_ref = unwrap!(drive.resolve_path(oldpath)).unwrap();
                let mut handles = get_live_handles().lock().unwrap();
                let mut guard = (**handles.get(&file_ref).unwrap()).lock().unwrap();
                let handle = guard.deref_mut() as *mut FileHandle;
                // close the file handle, but keep the allocation
                std::ptr::drop_in_place::<FileHandle>(handle);
                let result = drive.move_(file_ref, newpath);
                // re-open. the FileRef stays the same
                match drive.open(file_ref) {
                    Ok(new_handle) => {
                        // overwrite the old handle with the new one
                        std::ptr::write(handle, new_handle);
                        unwrap!(result);
                        0
                    }
                    Err(_) => {
                        eprintln!("fuse_rename: re-open failed after rename. expect a segfault.");
                        -libc::EIO
                    }
                }
            }
            Err(_) => -libc::EIO,
        }
    }
}

unsafe extern "C" fn fuse_truncate(path: *const c_char, size: off_t, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_truncate({:?}, {})", CStr::from_ptr(path), size);
        with_file_handle(path, fi, |handle| {
            handle.set_len(size as _);
            0
        })
    }
}

// note: since FileHandle implements only fread and fwrite, not read
// and write, we have to seek every time, which is inefficient.

unsafe extern "C" fn fuse_read(path: *const c_char, buf: *mut c_char, size: libc::size_t, offset: off_t, fi: *mut fuse_file_info) -> i32 {
    unsafe {
        eprintln!("fuse_read({:?}, {}, {})", CStr::from_ptr(path), size, offset);
        let mut handle = (*((*fi).fh as *const Mutex<FileHandle>)).lock().unwrap();
        let buf = std::slice::from_raw_parts_mut(buf as *mut u8, size);
        unwrap!(handle.seek(std::io::SeekFrom::Start(offset as u64)));
        unwrap!(handle.read(buf)) as _
    }
}

unsafe extern "C" fn fuse_write(path: *const c_char, buf: *const c_char, size: libc::size_t, offset: off_t, fi: *mut fuse_file_info) -> i32 {
    unsafe {
        eprintln!("fuse_write({:?}, {}, {})", CStr::from_ptr(path), size, offset);
        let mut handle = (*((*fi).fh as *const Mutex<FileHandle>)).lock().unwrap();
        let buf = std::slice::from_raw_parts(buf as *const u8, size);
        unwrap!(handle.seek(std::io::SeekFrom::Start(offset as u64)));
        unwrap!(handle.write(buf)) as _
    }
}

unsafe extern "C" fn fuse_chmod(path: *const c_char, mode: mode_t, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_chmod({:?}, {:o})", CStr::from_ptr(path), mode);
        // TODO: use user_attributes to store chmod info.
        0
    }
}

unsafe extern "C" fn fuse_chown(path: *const c_char, uid: uid_t, gid: gid_t, fi: *mut fuse_file_info) -> c_int {
    unsafe {
        eprintln!("fuse_chown({:?}, {}, {})", CStr::from_ptr(path), uid, gid);
        // TODO: use user_attributes to store chown info.
        0
    }
}

static FUSE: fuse_operations = fuse_operations {
    getattr: Some(fuse_getattr),
    readlink: None,
    mknod: None,
    mkdir: Some(fuse_mkdir),
    unlink: Some(fuse_unlink),
    rmdir: Some(fuse_unlink),
    symlink: None,
    rename: Some(fuse_rename),
    link: None,
    chmod: Some(fuse_chmod),
    chown: Some(fuse_chown),
    truncate: Some(fuse_truncate),
    open: Some(fuse_open),
    read: Some(fuse_read),
    write: Some(fuse_write),
    statfs: None,
    flush: None,
    release: Some(fuse_release),
    fsync: None,
    setxattr: None,
    getxattr: None,
    listxattr: None,
    removexattr: None,
    opendir: None,
    readdir: Some(fuse_readdir),
    releasedir: None,
    fsyncdir: None,
    init: Some(fuse_init),
    destroy: Some(fuse_destroy),
    access: None,
    create: Some(fuse_create),
    lock: None,
    utimens: Some(fuse_utimens),
    bmap: None,
    ioctl: None,
    poll: None,
    write_buf: None,
    read_buf: None,
    flock: None,
    fallocate: None,
    #[cfg(unix)]
    copy_file_range: None,
    #[cfg(unix)]
    lseek: None,
};