use std::sync::atomic::AtomicPtr;

use log::error;
use windows::{Win32::{Security::{ATTRIBUTE_SECURITY_INFORMATION, Authorization::SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SCOPE_SECURITY_INFORMATION}, Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY}}, core::PCSTR};
use winfsp::{U16CStr, U16CString};

pub(crate) fn get_security_descriptor(file_path: &[u8]) -> Result<&'static [u8], winfsp::FspError> { 
    unsafe {
        let mut security_desc_ptr = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let file_path = std::ffi::CString::new(file_path).unwrap();
        let res = windows::Win32::Security::Authorization::GetNamedSecurityInfoA(
            PCSTR(file_path.as_ptr() as _),
            SE_FILE_OBJECT, 
            ATTRIBUTE_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION | SCOPE_SECURITY_INFORMATION, 
            None, None, None, None, 
            &mut security_desc_ptr);
        if res.is_err() {
            return Err(winfsp::FspError::WIN32(res.0));
        }
        let security_desc_length = windows::Win32::Security::GetSecurityDescriptorLength(security_desc_ptr);
        Ok(std::slice::from_raw_parts(security_desc_ptr.0 as *const _, security_desc_length as _))
    } 
}

pub(crate) fn get_free_space_on_disk() -> Result<u64, windows::core::Error> {
    let mut avail = 0u64;
    unsafe { windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExA(
        PCSTR(b".\0".as_ptr()),
        Some(&mut avail), None, None)? 
    };
    Ok(avail)
}

// we use seconds since 1.1.1970, windows uses 100-nanosecond intervals since 1.1.1601
pub(crate) fn time_to_windows_time(unix_time: u64) -> u64 {
    (unix_time as u64 + 11644473600) * 10_000_000
}

pub(crate) fn windows_time_to_time(windows_time: u64) -> u64 {
    (windows_time as u64 / 10_000_000).saturating_sub(11644473600)
}

pub(crate) fn file_attributes_from_fileinfo(info: &farfs::fs::FileInfo) -> u32 {
    let mut attributes = 0;
    if info.is_directory { attributes |= FILE_ATTRIBUTE_DIRECTORY.0; }
    if info.is_readonly { attributes |= FILE_ATTRIBUTE_READONLY.0; }
    if attributes == 0 { attributes = FILE_ATTRIBUTE_NORMAL.0; }
    attributes
}

pub(crate) fn file_info_from_fileinfo(winfsp_fileinfo: &mut winfsp::filesystem::FileInfo, info: &farfs::fs::FileInfo) {
    *winfsp_fileinfo = winfsp::filesystem::FileInfo {
        file_attributes: file_attributes_from_fileinfo(info),
        allocation_size: info.size,
        file_size: info.size,
        creation_time: time_to_windows_time(info.create_time),
        last_access_time: info.access_time.map_or(0, time_to_windows_time),
        last_write_time: info.modify_time.map_or(0, time_to_windows_time),
        change_time: info.modify_time.map_or(0, time_to_windows_time),
        index_number: info.self_ref.into(),
        hard_links: 0, ea_size: 0, reparse_tag: 0,
    };
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct CreateOptions: u32 {
        const FILE_DIRECTORY_FILE = 0x00000001;
        const FILE_WRITE_THROUGH = 0x00000002;
        const FILE_SEQUENTIAL_ONLY = 0x00000004;
        const FILE_NO_INTERMEDIATE_BUFFERING = 0x00000008;
        const FILE_SYNCHRONOUS_IO_ALERT = 0x00000010;
        const FILE_SYNCHRONOUS_IO_NONALERT = 0x00000020;
        const FILE_NON_DIRECTORY_FILE = 0x00000040;
        const FILE_CREATE_TREE_CONNECTION = 0x00000080;
        const FILE_COMPLETE_IF_OPLOCKED = 0x00000100;
        const FILE_NO_EA_KNOWLEDGE = 0x00000200;
        const FILE_OPEN_REMOTE_INSTANCE = 0x00000400;
        const FILE_RANDOM_ACCESS = 0x00000800;
        const FILE_DELETE_ON_CLOSE = 0x00001000;
        const FILE_OPEN_BY_FILE_ID = 0x00002000;
        const FILE_OPEN_FOR_BACKUP_INTENT = 0x00004000;
        const FILE_NO_COMPRESSION = 0x00008000;
        const FILE_OPEN_REQUIRING_OPLOCK = 0x00010000;
        const FILE_RESERVE_OPFILTER = 0x00100000;
        const FILE_OPEN_REPARSE_POINT = 0x00200000;
        const FILE_OPEN_NO_RECALL = 0x00400000;
        const FILE_OPEN_FOR_FREE_SPACE_QUERY = 0x00800000;
    }

}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct FileAttributes: u32 {
        const READONLY = 0x00000001;
        const HIDDEN = 0x00000002;
        const SYSTEM = 0x00000004;
        const DIRECTORY = 0x00000010;
        const ARCHIVE = 0x00000020;
        const DEVICE = 0x00000040;
        const NORMAL = 0x00000080;
        const TEMPORARY = 0x00000100;
        const SPARSE_FILE = 0x00000200;
        const REPARSE_POINT = 0x00000400;
        const COMPRESSED = 0x00000800;
        const OFFLINE = 0x00001000;
        const NOT_CONTENT_INDEXED = 0x00002000;
        const ENCRYPTED = 0x00004000;
    }
}

#[track_caller]
pub(crate) fn log_err<T>(f: impl FnOnce() -> winfsp::Result<T>) -> winfsp::Result<T> {
    match f() {
        Ok(x) => Ok(x),
        Err(e) => {
            if !(e.to_ntstatus() as u32 == 0xC0000034) {
                // ignore "file not found" errors, they are expected in some cases
                let location = std::panic::Location::caller();
                error!("Error at {}: {}", location, debug_ntstatus(e.to_ntstatus()));
            }
            Err(e)
        }
    }
}

// Convert a Windows string to a byte slice.
pub(crate) fn winstr_to_bytes(wstr: &U16CStr) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buf = [0; 4];
    for char in wstr.chars() {
        match char {
            Ok(c) => {
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Err(err) => {
                let unpaired_surrogate = err.unpaired_surrogate();
                if unpaired_surrogate & 0xDF00 == 0xDC00 {
                    // assume this is our encoding of invalid bytes
                    bytes.push((unpaired_surrogate - 0xDC00) as u8);
                } else {
                    // no good way to handle this
                    bytes.extend_from_slice(&unpaired_surrogate.to_ne_bytes())
                }
            }
        }
    }
    bytes
}

pub(crate) fn bytes_to_winstr(bytes: &[u8]) -> U16CString {
    let mut wstr = Vec::new();
    for chunk in bytes.utf8_chunks() {
        for ch in chunk.valid().chars() {
            let mut buf = [0; 2];
            wstr.extend_from_slice(ch.encode_utf16(&mut buf));
        }
        for b in chunk.invalid() {
            // encode invalid bytes as unpaired surrogates
            wstr.push((*b as u16) + 0xDC00u16);
        }
    }
    U16CString::from_vec_truncate(wstr)
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

pub(crate) struct DispAsStr<'a>(pub(crate) &'a [u8]);
impl std::fmt::Display for DispAsStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ch in self.0.utf8_chunks() {
            write!(f, "{}", ch.valid())?;
            for b in ch.invalid() {
                write!(f, "\\x{:02x}", b)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn debug_ntstatus(status: i32) -> std::borrow::Cow<'static, str> {
    match status as u32 {
        0xC000000D => "INVALID_PARAMETER".into(),
        0xC000000F => "NO_SUCH_FILE".into(),
        0xC0000010 => "INVALID_DEVICE_REQUEST".into(),
        0xC0000022 => "ACCESS_DENIED".into(),
        0xC0000034 => "OBJECT_NAME_NOT_FOUND".into(),
        0xC0000035 => "OBJECT_NAME_COLLISION".into(),
        0xC000003A => "OBJECT_PATH_NOT_FOUND".into(),
        0xc0000467 => "FILE_NOT_AVAILABLE".into(),
        0xC00000E9 => "UNEXPECTED_IO_ERROR".into(),
        0xC0000101 => "DIRECTORY_NOT_EMPTY".into(),
        _ => format!("NTSTATUS({:#x})", status).into(),
    }
}