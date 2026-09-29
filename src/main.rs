use std::{io::{Read, Seek, Write}, os::windows::fs::OpenOptionsExt};

use filearchive2::{drive::{Drive, DriveConf, FileInfo, FileRef}, paging::PagedFile, util::FileLike};


fn main() {
    let mut f = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).open("test.drive").unwrap();

    let drive = Drive::new_create(f, DriveConf::default()).unwrap();

    drive.create_file("/home/test/greeting.txt")
        .unwrap()
        .write_all(b"Hello, world!\n").unwrap();
    drive.move_("/home/test/greeting.txt", "/home/docs/new_greeting.txt").unwrap();
    //drive.dbg_file_directory().unwrap();
    drive.delete("/home/docs/new_greeting.txt").unwrap();
    {
        let mut f = drive.create_file("/home/docs/new_greeting.txt")
        .unwrap();
        f.set_readonly(true);
    }
}

