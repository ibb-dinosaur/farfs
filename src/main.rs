use std::{io::{Read, Seek, Write}, os::windows::fs::OpenOptionsExt};

use filearchive2::{drive::{Drive, FileInfo, FileRef}, paging::PagedFile, util::FileLike};


fn main() {
    let mut f = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).open("test.drive").unwrap();
    
    let mut drive = drive::Drive::new_existing(PagedFile::new(f)).unwrap();
    println!("{:?}", drive.info(drive.root()));
    println!("{:?}", drive.dir_entries(drive.root()));

    let mut file1 = drive.create_file(drive.root(), b"test.txt").unwrap();
    println!("{:?}", file1.get_ref());
    println!("{:?}", drive.info(file1.get_ref()));
    file1.write_all(b"Hello, world!").unwrap();
    file1.flush().unwrap();
    println!("{:?}", drive.info(file1.get_ref()));
    println!("{:?}", drive.dir_entries(drive.root()));
}
