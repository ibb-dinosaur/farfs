use std::{cell::RefCell, fs::File};

pub trait FileLike {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
    fn stream_length(&self) -> std::io::Result<u64>;
    fn flush(&self) -> std::io::Result<()>;
    /// Try to acquire an exclusive lock on part of the file
    /// Returns true if the operation was successful or false if the section is locked at the moment and block=false was passed
    fn try_lock_part(&self, offset: u64, len: u64, block: bool, unlock: bool) -> std::io::Result<bool>;
}

#[cfg(windows)]
#[link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" {
    pub fn GetFileSizeEx(hfile: *mut core::ffi::c_void, lpfilesize: *mut i64) -> i32;
    pub fn LockFileEx(hfile : *mut core::ffi::c_void, dwflags : u32, dwreserved : u32, nnumberofbytestolocklow : u32, nnumberofbytestolockhigh : u32, lpoverlapped : *mut OVERLAPPED) -> i32;
    pub fn UnlockFileEx(hfile : *mut core::ffi::c_void, dwreserved : u32, nnumberofbytestolocklow : u32, nnumberofbytestolockhigh : u32, lpoverlapped : *mut OVERLAPPED) -> i32;      
}
#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED {
    pub internal: usize,
    pub internalhigh: usize,
    pub offset: u32,
    pub offsethigh: u32,
    pub pointer: *mut core::ffi::c_void,
    pub hevent: *mut core::ffi::c_void,
}

#[cfg(windows)]
impl FileLike for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::windows::fs::FileExt;
        let mut n = 0;
        while n < buf.len() {
            match self.seek_read(&mut buf[n..], offset + n as u64) {
                Ok(0) => break,
                Ok(readn) => {
                    n += readn;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        if n != buf.len() { 
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Failed to read exact number of bytes")) 
        } else { 
            Ok(()) 
        }
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        use std::os::windows::fs::FileExt;
        let mut n = 0;
        while n < buf.len() {
            match self.seek_write(&buf[n..], offset + n as u64) {
                Ok(0) => {
                    return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "Failed to write all bytes"));
                },
                Ok(written) => {
                    n += written;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn stream_length(&self) -> std::io::Result<u64> {
        use std::os::windows::io::AsRawHandle;
        let mut file_size: i64 = 0;
        let ok = unsafe { GetFileSizeEx(self.as_raw_handle(), &mut file_size) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file_size as u64)
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    
    fn try_lock_part(&self, offset: u64, len: u64, block: bool, unlock: bool) -> std::io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.offset = (offset & 0xFFFFFFFF) as u32;
        overlapped.offsethigh = (offset >> 32) as u32;
        // we don't care about the other `OVERLAPPED` struct fields
        if !unlock {
            let ok = unsafe { LockFileEx(
            self.as_raw_handle(),
            if block { 2 } else { 3 },
            0, (len & 0xFFFFFFFF) as u32,
            (offset >> 32) as u32, &mut overlapped as *mut _) };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                if !block && err.raw_os_error() == Some(997) {
                    Ok(false) // lock held by someone else
                } else {
                    Err(err)
                }
            } else {
                Ok(true) // lock acquired
            }
        } else {
            let ok = unsafe { UnlockFileEx(self.as_raw_handle(), 0, (len & 0xFFFFFFFF) as u32, (offset >> 32) as u32, &mut overlapped as *mut _) };
            if ok == 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(true)
            }
        }
    }
    
    
}

#[cfg(unix)]
impl FileLike for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        <Self as FileExt>::read_exact_at(self, buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        <Self as FileExt>::write_all_at(self, buf, offset)
    }
    fn stream_length(&self) -> std::io::Result<u64> {
        use std::os::fd::AsRawFd;
        match unsafe { libc::lseek(self.as_raw_fd(), 0, libc::SEEK_END) } {
            -1 => Err(std::io::Error::last_os_error()),
            n => Ok(n as u64),
        }
    }
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    
    fn try_lock_part(&self, offset: u64, len: u64, block: bool, unlock: bool) -> std::io::Result<bool> {
        use std::os::fd::AsRawFd;
        let flock = libc::flock {
            l_type: if unlock { libc::F_UNLCK as _ } else { libc::F_WRLCK as _ /* exclusive lock */ },
            l_whence: libc::SEEK_SET as _,
            l_start: offset as _,
            l_len: len as _,
            l_pid: 0,
        };
        if block {
            let ok = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETLKW, &flock as *const libc::flock) };
            if ok == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(true)
            }
        } else {
            let ok = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETLK, &flock as *const libc::flock) };
            if ok == -1 {
                let err = std::io::Error::last_os_error();
                if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied) {
                    Ok(false) // lock held by someone else
                } else {
                    Err(err)
                }
            } else {
                Ok(true)
            }
        }
    }
}

impl FileLike for RefCell<Vec<u8>> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        let this = self.borrow();
        if offset as usize + buf.len() > this.len() {
            Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Failed to read exact number of bytes"))
        } else {
            buf.copy_from_slice(&this[offset as usize .. offset as usize + buf.len()]);
            Ok(())
        }
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        let mut this = self.borrow_mut();
        if offset as usize + buf.len() > this.len() {
            this.resize(offset as usize + buf.len(), 0);
        }
        this[offset as usize .. offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn stream_length(&self) -> std::io::Result<u64> {
        Ok(self.borrow().len() as u64)
    }

    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    
    fn try_lock_part(&self, _offset: u64, _len: u64, _block: bool, _unlock: bool) -> std::io::Result<bool> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "Locking is not supported on in-memory bytes"))
    }
}
