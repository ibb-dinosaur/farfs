use std::{io::{Read, Seek, Write}};

use crate::util::FileLike;

mod paging;
mod archive;
mod util;

fn main() {
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).open("testfile.bin").unwrap();
    let pf = paging::PagedFile::new(f);
    let mut b1 = pf.open_existing_block(0).unwrap();
    b1.dbg();
    b1.write(&[b'A'; 3000]).unwrap();
    b1.dbg();
    b1.seek(std::io::SeekFrom::Start(0)).unwrap();
    b1.dbg();
    let mut buffer = [0; 2500];
    b1.read(&mut buffer).unwrap();
    b1.dbg();
}
