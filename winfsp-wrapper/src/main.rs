use std::{io::{Read, Seek, SeekFrom, Write}, sync::{Mutex, atomic::AtomicBool}};

use farfs::fs::{FileHandle, FileRef};
use windows::Win32::{Foundation::{STATUS_FILE_NOT_AVAILABLE, STATUS_INVALID_DEVICE_REQUEST, STATUS_OBJECT_NAME_NOT_FOUND}, Storage::FileSystem::INVALID_FILE_ATTRIBUTES};
use winfsp::{filesystem::WideNameInfo, host::{CoarseGuard, DebugMode, FileSystemHost, FileSystemParams, VolumeParams}};
use log::{info, trace};

use crate::util::*;

mod util;

fn main() {
    /*let dr = farfs::fs::Drive::new_existing(std::fs::OpenOptions::new().read(true).write(true)
        .open("backing.drive").unwrap()).unwrap();
    dr.dump_cft().unwrap();
    return;*/

    /*farfs::paging::PagedFile::new(std::fs::OpenOptions::new().read(true).write(true)
        .open("backing.drive").unwrap(), 2048).dump_pages().unwrap();
    return;*/

    let logfile = std::fs::File::create(
          format!(".\\{}.log", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs())).unwrap(
    );
    simplelog::WriteLogger::init(simplelog::LevelFilter::Trace, simplelog::Config::default(), logfile).unwrap();

    let drive_path = ".\\backing.drive";

    let init = winfsp::winfsp_init().unwrap();
    let mut service = 
        winfsp::service::FileSystemServiceBuilder::new()
        .with_start(|| {
            let drive = farfs::fs::Drive::new_existing(std::fs::OpenOptions::new().read(true).write(true).open("backing.drive")?)?;
            let stats = drive.drive_stats()?;
            let mut params = VolumeParams::new();
            params.prefix("")
                .filesystem_name(str::from_utf8(&*stats.drive_name).unwrap_or(""))
                .sectors_per_allocation_unit(stats.page_size.min(65535) as u16)
                .sector_size((stats.page_size / stats.page_size.min(65535)) as u16)
                .max_component_length(255)
                .volume_creation_time(drive.info(drive.root())?.map_or_default(|f| time_to_windows_time(f.create_time)))
                //.volume_serial_number(n)
                .irp_timeout(60000).irp_capacity(100)
                .file_info_timeout(1000) // 1 second timeout
                .volume_info_timeout(60000)
                .security_timeout(600000) // security info never changes
                .case_sensitive_search(true)
                .case_preserved_names(true)
                .named_streams(false).extended_attributes(false).read_only_volume(false)
                .reparse_points(false).reparse_points_access_check(false);

            let dbg = DebugMode::none(); //DebugMode::all();
            let mut host = 
                FileSystemHost::<Fs, CoarseGuard>::new_with_options(FileSystemParams::default_params_debug(params, dbg), 
                Fs{
                    drive: Box::new(drive),
                    sd: get_security_descriptor(b".")?,
                    disk_free_space: get_free_space_on_disk()?,
                    handles: lockfree::map::Map::new(),
                })?;
            host.mount("X:")?;
            host.start()?;
            Ok(host)
        })
        .with_stop(|host| {
            if let Some(host) = host {
                host.stop();
            }
            Ok(())
        })
        .build("fstest", init).unwrap();
    service.start().unwrap();
    service.join().unwrap();
}
pub struct Fs {
    drive: Box<farfs::fs::Drive>, // make sure Drive is not moved or dropped
    // key: file uids
    handles: lockfree::map::Map<u64, (Mutex<farfs::fs::FileHandle<'static>>, AtomicBool)>,
    sd: &'static [u8],
    disk_free_space: u64,
}

impl Fs {
    fn with_handle<T>(&self, fr: FileRef, f: impl FnOnce(&mut farfs::fs::FileHandle) -> winfsp::Result<T>) -> winfsp::Result<T> {
        match self.handles.get(&fr.into()) {
            Some(guard) => { 
                let mut file_handle = guard.1.0.lock().unwrap();
                f(&mut file_handle)
            }
            None => {
                let mut handle = self.drive.open(fr)?;
                f(&mut handle)
            }
        }
    }
}

impl winfsp::filesystem::FileSystemContext for Fs {
    /// index into Fs::handles
    type FileContext = u64;

    fn get_security_by_name(
        &self,
        file_name: &winfsp::U16CStr,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
        _reparse_point_resolver: impl FnOnce(&winfsp::U16CStr) -> Option<winfsp::filesystem::FileSecurity>,
    ) -> winfsp::Result<winfsp::filesystem::FileSecurity> { log_err(|| {
        let file_name = winstr_to_bytes(file_name);
        trace!("get_security_by_name({})", DispAsStr(&file_name));
        let fr = self.drive.resolve_path(file_name)?;
        match fr.map(|f| self.drive.info(f)).transpose()? {
            Some(Some(fileinfo)) => {
                // copy generic file descriptor to the output buffer
                let mut sz_security_descriptor = 0;
                if let Some(security_descriptor) = security_descriptor {
                    sz_security_descriptor = self.sd.len().min(security_descriptor.len());
                    unsafe { std::ptr::copy(self.sd.as_ptr(), security_descriptor.as_mut_ptr() as *mut u8, sz_security_descriptor) };
                }
                Ok(winfsp::filesystem::FileSecurity {
                    reparse: false,
                    sz_security_descriptor: sz_security_descriptor as _,
                    attributes: file_attributes_from_fileinfo(&fileinfo),
                })
            }
            _ => Err(STATUS_OBJECT_NAME_NOT_FOUND.into())
        }
    } )}

    fn open(
        &self,
        file_name: &winfsp::U16CStr,
        create_options: u32, _: u32,
        open_file_info: &mut winfsp::filesystem::OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> { log_err(|| {
        let file_name = winstr_to_bytes(file_name);
        trace!("open({}, {:?})", DispAsStr(&file_name), CreateOptions::from_bits_retain(create_options));
        let fr = self.drive.resolve_path(&file_name)?.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
        let fileinfo = self.drive.info(fr)?.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
        file_info_from_fileinfo(open_file_info.as_mut(), &fileinfo);
        let delete_on_close = create_options & CreateOptions::FILE_DELETE_ON_CLOSE.bits() != 0;
        if !fileinfo.is_directory {
            match self.handles.get(&fr.into()) {
                Some(guard) => { 
                    // already opened
                    if delete_on_close {
                        guard.1.1.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                None => {
                    let handle = self.drive.open(fr)?;
                    let m = (
                        Mutex::new(unsafe { std::mem::transmute(handle) }), delete_on_close.into());
                    self.handles.insert(fr.into(), m);
                }
            }
        }
        Ok(fr.into())
    } )}

    fn close(&self, context: Self::FileContext) { let _ = log_err(|| {
        trace!("close({:?})", context);
        match self.handles.remove(&context) {
            Some(removed) => {
                let (mut file_handle, delete_flag) = spin_until_removed(removed);
                let fr = file_handle.get_mut().unwrap().get_ref();
                file_handle.into_inner().unwrap().close()?;
                if delete_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    self.drive.delete(fr)?;
                }
                Ok(())
            }
            _ => Ok(())
        }
    } ); }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut winfsp::filesystem::FileInfo) -> winfsp::Result<()> { log_err(|| {
        trace!("get_file_info({:?})", context);
        let fileinfo = match self.handles.get(context) {
            Some(file_handle) => {
                file_handle.1.0.lock().unwrap().full_fileinfo()?
            },
            None => {
                self.drive.info(Into::<FileRef>::into(*context))?.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?
            },
        };
        file_info_from_fileinfo(file_info, &fileinfo);
        Ok(())
    } )}

    fn get_volume_info(&self, out_volume_info: &mut winfsp::filesystem::VolumeInfo) -> winfsp::Result<()> { log_err(|| {
        trace!("get_volume_info()");
        use std::os::windows::ffi::OsStringExt;
        let drive_stats = self.drive.drive_stats()?;
        out_volume_info.total_size = drive_stats.used_pages * drive_stats.page_size;
        out_volume_info.free_size = self.disk_free_space;
        out_volume_info.set_volume_label(std::ffi::OsString::from_wide(bytes_to_winstr(&drive_stats.drive_name).as_slice()));
        Ok(())
    } )}

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&winfsp::U16CStr>, // ignore: pattern matching is done by the caller
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> { log_err(|| {
        trace!("read_directory({:?}, {:?})", context, marker);
        let file_ref = Into::<FileRef>::into(*context);
        let entries = self.drive.dir_entries(file_ref)?;
        let marker = marker.inner_as_cstr().map(winstr_to_bytes);
        let mut is_past_marker = marker.is_none();
        let mut buffer_cursor = 0;
        let mut dirinfo_buffer = winfsp::filesystem::DirInfo::<255>::new();
        for child in entries {
            if let Some(child_info) = self.drive.info(child)? {
                if !is_past_marker {
                    if let Some(m) = &marker && 
                       farfs::util::compare_case_insensitive(&child_info.name, &**m) {
                        is_past_marker = true;
                    }
                    continue;
                }

                file_info_from_fileinfo(dirinfo_buffer.file_info_mut(), &child_info);
                dirinfo_buffer.set_name_cstr(bytes_to_winstr(&child_info.name))?;
                if !dirinfo_buffer.append_to_buffer(buffer, &mut buffer_cursor) {
                    break;
                }
            }
        }
        winfsp::filesystem::DirInfo::<255>::finalize_buffer(buffer, &mut buffer_cursor);
        Ok(buffer_cursor)
    } )}
    
    fn create(
        &self,
        file_name: &winfsp::U16CStr,
        create_options: u32, _granted_access: u32,
        _: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        allocation_size: u64, _extra_buffer: Option<&[u8]>, _extra_buffer_is_reparse_point: bool,
        file_info: &mut winfsp::filesystem::OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> { log_err(|| {
        let create_options = CreateOptions::from_bits_retain(create_options);
        let file_name = winstr_to_bytes(file_name);
        info!("create({}, {:?}, {:?})", DispAsStr(&file_name), create_options, allocation_size);
        let is_directory = create_options.contains(CreateOptions::FILE_DIRECTORY_FILE);
        if is_directory {
            let fref = self.drive.create_directory(&*file_name)?;
            let finfo = self.drive.info(fref)?.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
            file_info_from_fileinfo(file_info.as_mut(), &finfo);
            Ok(fref.into())
        } else {
            let fhndl = self.drive.create_file(&*file_name)?;
            let fref = fhndl.get_ref();
            let finfo = fhndl.full_fileinfo()?;
            file_info_from_fileinfo(file_info.as_mut(), &finfo);
            let m = (Mutex::new(unsafe { std::mem::transmute(fhndl) }), false.into());
            self.handles.insert(fref.into(), m);
            Ok(fref.into())
        }
    } )}

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> { log_err(|| {
        info!("set_file_size({:?}, {:?}, {:?},)", context, new_size, set_allocation_size);
        if set_allocation_size { return Ok(()); } // ignore allocation sizes
        self.with_handle((*context).into(), |file_handle| {
            file_handle.set_len(new_size)?;
            let finfo = file_handle.full_fileinfo()?;
            file_info_from_fileinfo(file_info, &finfo);
            Ok(())
        })
    } )}

    fn get_security(
        &self,
        context: &Self::FileContext,
        security_descriptor: Option<&mut [std::ffi::c_void]>,
    ) -> winfsp::Result<u64> { log_err(|| {
        trace!("get_security({:?})", context);
        let mut sz_security_descriptor = 0;
        if let Some(security_descriptor) = security_descriptor {
            sz_security_descriptor = self.sd.len().min(security_descriptor.len());
            unsafe { std::ptr::copy(self.sd.as_ptr(), security_descriptor.as_mut_ptr() as *mut u8, sz_security_descriptor) };
        }
        Ok(sz_security_descriptor as _)
    } )}

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut winfsp::filesystem::FileInfo) -> winfsp::Result<()> { log_err(|| {
        trace!("flush({:?})", context);
        match context {
            // flushing the entire volume is not supported
            None => Err(STATUS_INVALID_DEVICE_REQUEST.into()),
            Some(context) => {
                match self.handles.get(context) {
                   Some(guard) => {
                        let mut file_handle = guard.1.0.lock().unwrap();
                        file_handle.flush()?;
                        let finfo = file_handle.full_fileinfo()?;
                        file_info_from_fileinfo(file_info, &finfo);
                        Ok(())
                    }
                    _ => Ok(()) // flushing a non-open file means nothing
                }
            }
        }
    } )}

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> { log_err(|| {
        info!("read({:?}, {:?}, n={})", context, offset, buffer.len());
        self.with_handle((*context).into(), |file| {
            file.seek(SeekFrom::Start(offset))?;
            let n = file.read(buffer)?;
            Ok(n as _)
        })
    } )}

    fn write(&self,
        context: &Self::FileContext,
        mut buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<u32> { log_err(|| {
        info!("write({:?}, {:?}, n={}, {:?}, {:?})", context, offset, buffer.len(), write_to_eof, constrained_io);
        self.with_handle((*context).into(), |file| {
            if write_to_eof {
                file.seek(SeekFrom::End(0))?;
            } else {
                file.seek(SeekFrom::Start(offset))?;
            }
            if constrained_io { // we can't extend the file
                buffer = &buffer[..buffer.len().min((file.file_size() - file.stream_position()?) as _)];
            }
            let n = file.write(buffer)?;
            let finfo = file.full_fileinfo()?;
            file_info_from_fileinfo(file_info, &finfo);
            Ok(n as _)
        })
    } )}
    
    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&winfsp::U16CStr>, flags: u32) {
        trace!("cleanup({:?}, {:?}, {:?})", context, file_name, flags);
        // we don't do anything on cleanup
    }
    
    fn overwrite(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        replace_file_attributes: bool,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> { log_err(|| {
        let file_attributes = FileAttributes::from_bits_retain(file_attributes);
        info!("overwrite({:?}, {:?}, {:?}, {:?}, {:?})", context, file_attributes, replace_file_attributes, allocation_size, file_info);
        self.with_handle((*context).into(), |file| {
            file.set_len(0)?; // truncate the file
            if replace_file_attributes {
                file.set_readonly(file_attributes.contains(FileAttributes::READONLY));
            } else {
                let r = file.is_readonly() || file_attributes.contains(FileAttributes::READONLY);
                file.set_readonly(r);
            }
            let finfo = file.full_fileinfo()?;
            file_info_from_fileinfo(file_info, &finfo);
            Ok(())
        })
    } )}
    
    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        mut last_change_time: u64,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> { log_err(|| {
        let file_attributes = FileAttributes::from_bits_retain(file_attributes);
        info!("set_basic_info({:?}, {:?}, {:?}, {:?}, {:?}, {:?}, {:?})", context, file_attributes, creation_time, last_access_time, last_write_time, last_change_time, file_info);
        match self.handles.get(context) {
            None => {
                let file_ref = Into::<FileRef>::into(*context);
                self.drive.alter(file_ref,
                    if file_attributes.bits() != 0 { Some(file_attributes.contains(FileAttributes::READONLY)) } else { None },
                    if last_access_time != 0 { Some(windows_time_to_time(last_access_time)) } else { None },
                    if last_change_time != 0 { Some(windows_time_to_time(last_change_time)) } else { None },
                    None)?;
                let finfo = self.drive.info(file_ref)?.ok_or(STATUS_OBJECT_NAME_NOT_FOUND)?;
                file_info_from_fileinfo(file_info, &finfo);
                Ok(()) 
            },
            Some(guard) => {
                let mut file = guard.1.0.lock().unwrap();
                if !(file_attributes.bits() == 0 || file_attributes.bits() == INVALID_FILE_ATTRIBUTES) {
                    // 0x7ff7 means every attribute is set
                    file.set_readonly(file_attributes.contains(FileAttributes::READONLY));
                }
                // we don't support changing creation time
                if last_access_time != 0 {
                    file.set_access_time(windows_time_to_time(last_access_time));
                }
                last_change_time = last_change_time.max(last_write_time);
                if last_change_time != 0 {
                    file.set_modify_time(windows_time_to_time(last_change_time));
                }
                let finfo = file.full_fileinfo()?;
                file_info_from_fileinfo(file_info, &finfo);
                Ok(())
            },
        }
    } )}
    
    fn set_delete(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> { log_err(|| {
        info!("set_delete({:?}, {}, {:?})", context, DispAsStr(&winstr_to_bytes(file_name)), delete_file);
        match self.handles.get(context) {
            Some(guard) =>
                guard.1.1.store(delete_file, std::sync::atomic::Ordering::SeqCst),
            None => {
                if *context == 0 {
                    return Err(STATUS_INVALID_DEVICE_REQUEST.into());
                }
                self.drive.delete(Into::<FileRef>::into(*context))?;
            }
        };
        Ok(())
    } )}

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        new_path: &winfsp::U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> { log_err(|| {
        let new_path = winstr_to_bytes(new_path);
        info!("rename({:?}, {}, {}, {:?})", context, DispAsStr(&winstr_to_bytes(_file_name)), DispAsStr(&new_path), replace_if_exists);
        if replace_if_exists {
            if let Some(fr) = self.drive.resolve_path(&new_path)? {
                self.drive.delete(fr)?;
                // TODO: if moving fails, we should restore the deleted file
            }
        }
        let i = new_path.iter().rev().position(|&c| c == b'\\').ok_or(STATUS_INVALID_DEVICE_REQUEST)?;
        let (new_path_dir, new_file_name) = new_path.split_at(new_path.len() - i);
        let new_path_dir = self.drive.resolve_path(new_path_dir)?.ok_or(STATUS_INVALID_DEVICE_REQUEST)?;
        match self.handles.get(context) {
            None => {
                self.drive.move_(Into::<FileRef>::into(*context), (new_path_dir, new_file_name))?;
                Ok(())
            },
            Some(guard) => {
                let mut lock = guard.1.0.lock().unwrap();
                let fr = lock.get_ref();
                let file_handle_ptr = &mut *lock as *mut _;
                // we don't support moving open files, so we have to close and reopen it
                // SAFETY: we are holding the lock, so no one else can access the dropped handle
                unsafe { std::ptr::drop_in_place(file_handle_ptr) }; // close the handle
                let res = self.drive.move_(fr, (new_path_dir, new_file_name));
                // re-open whether the move succeeded or not, to keep the handle valid
                let new_handle = self.drive.open(fr)?;
                unsafe { std::ptr::write(file_handle_ptr, std::mem::transmute::<_, FileHandle<'static>>(new_handle)) };
                res?;
                Ok(())
            },
        }
    } )}
}


/*
    // --- unsupported operations ---

    fn set_security(
        &self,
        context: &Self::FileContext,
        security_information: u32,
        _modification_descriptor: winfsp::filesystem::ModificationDescriptor,
    ) -> winfsp::Result<()> {
        info!("set_security({:?}, {:?}) [UNSUPPORTED]", context, security_information);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn get_dir_info_by_name(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        _out_dir_info: &mut winfsp::filesystem::DirInfo,
    ) -> winfsp::Result<()> {
        info!("get_dir_info_by_name({:?}, {:?}) [UNSUPPORTED]", context, file_name);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn set_volume_label(&self, volume_label: &winfsp::U16CStr, _volume_info: &mut winfsp::filesystem::VolumeInfo) -> winfsp::Result<()> {
        info!("set_volume_label({:?}) [UNSUPPORTED]", volume_label);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn get_stream_info(&self, context: &Self::FileContext, _buffer: &mut [u8]) -> winfsp::Result<u32> {
        info!("get_stream_info({:?}) [UNSUPPORTED]", context);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn get_reparse_point_by_name(
        &self,
        file_name: &winfsp::U16CStr,
        is_directory: bool,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        info!("get_reparse_point_by_name({:?}, {:?}) [UNSUPPORTED]", file_name, is_directory);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        info!("get_reparse_point({:?}, {:?}) [UNSUPPORTED]", context, file_name);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn set_reparse_point(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        info!("set_reparse_point({:?}, {:?}) [UNSUPPORTED]", context, file_name);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn delete_reparse_point(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        info!("delete_reparse_point({:?}, {:?}) [UNSUPPORTED]", context, file_name);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn get_extended_attributes(
        &self,
        context: &Self::FileContext,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        info!("get_extended_attributes({:?}) [UNSUPPORTED]", context);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn set_extended_attributes(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        _file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        info!("set_extended_attributes({:?}, {:?}) [UNSUPPORTED]", context, buffer);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
    
    fn control(
        &self,
        context: &Self::FileContext,
        control_code: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> winfsp::Result<u32> {
        info!("control({:?}, {:?}, {:?}, {:?}) [UNSUPPORTED]", context, control_code, input, output);
        Err(STATUS_INVALID_DEVICE_REQUEST.into())
    }
 */