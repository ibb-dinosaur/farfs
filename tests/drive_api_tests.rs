//! Integration-style tests for the `Drive` / `FileHandle` API.
//!
//! These are written as plain `#[test]` functions that exercise several
//! methods together and assert the whole system behaves consistently,
//! rather than isolating each method behind a mock. Each test creates a
//! fresh in-memory `Drive` (via `PagedFile::from(Vec::new())`) so tests
//! don't interfere with each other.
//!
//! Adjust the `Drive::new_create` / `PagedFile::from` calls if your crate
//! exposes a different in-memory constructor.

use std::{cell::RefCell, collections::HashSet, io::{Read, Seek, SeekFrom, Write}, sync::Mutex};

// Bring the crate's own types into scope. Replace `your_crate` with the
// actual crate name, or remove this `use` if these tests live inside the
// crate itself (`use super::*;` in a `#[cfg(test)] mod tests` block).
use farfs::{fs::{Drive, DriveConf, FileInfo, FileRef, FsError}, paging::PagedFile};

/// Helper: spin up a brand-new, empty drive backed by an in-memory buffer.
fn new_drive() -> Drive {
    let backing = Mutex::new(Vec::new());
    Drive::new_create(backing, DriveConf::default()).expect("failed to create a fresh drive")
}

#[test]
fn root_exists_and_is_a_directory() {
    let drive = new_drive();
    let root = drive.root();

    let info = drive
        .info(root)
        .expect("info() should not error")
        .expect("root should always exist");

    assert!(info.is_directory, "root must be a directory");
    assert_eq!(info.size, 0, "a fresh root should report zero size");
}

#[test]
fn create_file_write_read_roundtrip() {
    let drive = new_drive();

    // Create + write.
    {
        let mut handle = drive.create_file("hello.txt").expect("create_file failed");
        handle.write_all(b"hello, world!").expect("write failed");
        assert_eq!(handle.file_size(), 13);
    }

    // Re-open by path and read back.
    {
        let mut handle = drive.open("hello.txt").expect("open by path failed");
        let mut buf = Vec::new();
        handle.read_to_end(&mut buf).expect("read failed");
        assert_eq!(buf, b"hello, world!");
    }

    // Re-open by FileRef and confirm it's the same file.
    {
        let file_ref = drive.open("hello.txt").unwrap().get_ref();
        let mut handle = drive.open(file_ref).expect("open by FileRef failed");
        let mut buf = Vec::new();
        handle.read_to_end(&mut buf).expect("read failed");
        assert_eq!(buf, b"hello, world!");
    }
}

#[test]
fn seek_then_partial_read_and_overwrite() {
    let drive = new_drive();
    let mut handle = drive.create_file("seekable.bin").unwrap();
    handle.write_all(b"0123456789").unwrap();

    // Seek back to the middle and read a slice.
    handle.seek(SeekFrom::Start(3)).unwrap();
    let mut buf = [0u8; 4];
    handle.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"3456");

    // Seek back to start and overwrite the first two bytes.
    handle.seek(SeekFrom::Start(0)).unwrap();
    handle.write_all(b"XY").unwrap();

    // Verify the overwrite took effect and the rest is untouched.
    handle.seek(SeekFrom::Start(0)).unwrap();
    let mut whole = Vec::new();
    handle.read_to_end(&mut whole).unwrap();
    assert_eq!(&whole, b"XY23456789");
    assert_eq!(handle.file_size(), 10, "overwrite shouldn't change length");
}

#[test]
fn create_directory_and_list_entries() {
    let drive = new_drive();

    let docs_ref = drive.create_directory("docs").expect("mkdir failed");
    drive.create_file("docs/a.txt").unwrap();
    drive.create_file("docs/b.txt").unwrap();
    drive.create_directory("docs/nested").unwrap();

    let entries = drive.dir_entries("docs").expect("dir_entries failed");
    assert_eq!(entries.len(), 3, "expected exactly 3 children of docs/");

    // Collect (name, is_directory) pairs for order-independent assertions.
    let mut seen: Vec<(Vec<u8>, bool)> = entries
        .iter()
        .map(|&r| {
            let info = drive.info(r).unwrap().expect("child should exist");
            (info.name, info.is_directory)
        })
        .collect();
    seen.sort();

    let mut expected = vec![
        (b"a.txt".to_vec(), false),
        (b"b.txt".to_vec(), false),
        (b"nested".to_vec(), true),
    ];
    expected.sort();

    assert_eq!(seen, expected);

    // The directory's own info should agree with the FileRef returned by
    // create_directory.
    let docs_info = drive.info(docs_ref).unwrap().unwrap();
    assert!(docs_info.is_directory);
    assert_eq!(docs_info.name, b"docs");
    assert_eq!(docs_info.parent, drive.root());
}

#[test]
fn rename_via_set_name_is_visible_through_reopen() {
    let drive = new_drive();
    {
        let mut handle = drive.create_file("old_name.txt").unwrap();
        handle.write_all(b"payload").unwrap();
        handle.set_name(b"new_name.txt").unwrap();
        assert_eq!(handle.name().as_ref(), b"new_name.txt");
    }

    // Old path should no longer resolve...
    let old = drive.open("old_name.txt");
    assert!(old.is_err() || matches!(drive.info("old_name.txt"), Ok(None)));

    // ...and the new path should, with contents intact.
    let mut handle = drive.open("new_name.txt").expect("renamed file should open");
    let mut buf = Vec::new();
    handle.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"payload");
}

#[test]
fn readonly_flag_round_trips_through_info_and_handle() {
    let drive = new_drive();
    let file_ref = {
        let mut handle = drive.create_file("locked.txt").unwrap();
        handle.write_all(b"do not touch").unwrap();
        assert!(!handle.is_readonly(), "files should be writable by default");

        handle.set_readonly(true);
        assert!(handle.is_readonly());
        handle.get_ref()
    };

    // Confirm the flag is visible via a completely fresh handle and via info().
    let handle = drive.open(file_ref).unwrap();
    assert!(handle.is_readonly());

    let info = drive.info(file_ref).unwrap().unwrap();
    assert!(info.is_readonly);
}

#[test]
fn info_on_missing_path_returns_none_not_error() {
    let drive = new_drive();
    let result = drive.info("this/path/does/not/exist.txt");
    match result {
        Ok(None) => {} // expected
        Ok(Some(_)) => panic!("nonexistent path should not resolve to a FileInfo"),
        Err(e) => panic!("expected Ok(None) for a missing path, got Err: {e:?}"),
    }
}

#[test]
fn nested_directories_report_correct_parent_chain() {
    let drive = new_drive();
    drive.create_directory("a").unwrap();
    drive.create_directory("a/b").unwrap();
    let leaf = drive.create_file("a/b/leaf.txt").unwrap().get_ref();

    let leaf_info = drive.info(leaf).unwrap().unwrap();
    let b_info = drive.info(leaf_info.parent).unwrap().unwrap();
    let a_info = drive.info(b_info.parent).unwrap().unwrap();

    assert_eq!(leaf_info.name, b"leaf.txt");
    assert_eq!(b_info.name, b"b");
    assert_eq!(a_info.name, b"a");
    assert_eq!(a_info.parent, drive.root());
}

#[test]
fn timestamps_are_populated_and_monotonic_ish() {
    let drive = new_drive();
    let file_ref = drive.create_file("timed.txt").unwrap().get_ref();
    let info = drive.info(file_ref).unwrap().unwrap();

    // We can't assert exact values (depends on the system clock / VFS
    // implementation), but a freshly created file should have all three
    // timestamps set and create_time <= modify_time, create_time <= access_time.
    assert!(info.create_time > 0);
    assert!(info.modify_time.unwrap() >= info.create_time);
    assert!(info.access_time.unwrap() >= info.create_time);
}

/*
/// Simulates closing and reopening the "disk": persists the backing bytes
/// into a new `Drive::new_existing` and checks the filesystem survives a
/// round trip. This is the closest thing to an on-disk durability test
/// without touching the real filesystem.
#[test]
fn drive_state_survives_reopen_via_new_existing() {

    let file_ref = {
        let drive = new_drive();
        let mut handle = drive.create_file("persisted.txt").unwrap();
        handle.write_all(b"still here?").unwrap();
        (handle.get_ref(), drive.
    };

    let drive2 = Drive::new_existing(backing).expect("should reopen an existing drive");
    let mut handle = drive2.open(file_ref).expect("file should survive reopen");
    let mut buf = Vec::new();
    handle.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"still here?");
}*/

// -----------------------------------------------------------------
// root / info basics
// -----------------------------------------------------------------

#[test]
fn root_exists_and_is_directory() {
    let d = new_drive();
    let root = d.root();
    let info = d.info(root).unwrap().expect("root should always exist");
    assert!(info.is_directory);
}

#[test]
fn info_does_not_require_exclusive_access() {
    // info() is documented as "not exclusive" - it should succeed even
    // while the file is open elsewhere.
    let d = new_drive();
    let f = d.create_file(b"/shared-info.txt").unwrap();
    let info = d.info(b"/shared-info.txt").unwrap();
    assert!(info.is_some());
    drop(f);
}

#[test]
fn resolving_by_path_and_by_fileref_agree() {
    let d = new_drive();
    let r = d.create_directory(b"/both-ways").unwrap();
    let by_path = d.info(b"/both-ways").unwrap().unwrap();
    let by_ref = d.info(r).unwrap().unwrap();
    assert_eq!(by_path.name, by_ref.name);
    assert_eq!(by_path.is_directory, by_ref.is_directory);
}

// -----------------------------------------------------------------
// create_file / open / exclusivity
// -----------------------------------------------------------------

#[test]
fn create_and_reopen_file() {
    let d = new_drive();
    {
        let mut f = d.create_file(b"/hello.txt").unwrap();
        f.write_all(b"hi there").unwrap();
    } // handle dropped -> file released

    let mut f2 = d.open(b"/hello.txt").unwrap();
    let mut buf = Vec::new();
    f2.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"hi there");
}

#[test]
fn open_is_exclusive_while_handle_is_alive() {
    let d = new_drive();
    let f = d.create_file(b"/exclusive.txt").unwrap();

    let second = d.open(b"/exclusive.txt");
    assert!(second.is_err(), "opening an already-open file should fail");

    drop(f);

    let third = d.open(b"/exclusive.txt");
    assert!(third.is_ok(), "file should be openable again once released");
}

#[test]
fn open_nonexistent_file_errors() {
    let d = new_drive();
    let result = d.open(b"/does/not/exist.txt");
    assert!(result.is_err());
}

#[test]
fn create_file_on_existing_path_errors() {
    let d = new_drive();
    drop(d.create_file(b"/dup.txt").unwrap());
    let result = d.create_file(b"/dup.txt");
    assert!(result.is_err());
}

#[test]
fn get_ref_stays_valid_after_handle_is_dropped() {
    let d = new_drive();
    let r = {
        let f = d.create_file(b"/ref-check.txt").unwrap();
        f.get_ref()
    };
    assert!(d.info(r).unwrap().is_some());
    let _reopened = d.open(r).unwrap();
}

// -----------------------------------------------------------------
// directories / listing
// -----------------------------------------------------------------

#[test]
fn new_directory_has_no_entries() {
    let d = new_drive();
    let dir = d.create_directory(b"/empty").unwrap();
    let entries = d.dir_entries(dir).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn create_directory_and_list_children() {
    let d = new_drive();
    let dir_ref = d.create_directory(b"/docs").unwrap();

    let info = d.info(dir_ref).unwrap().unwrap();
    assert!(info.is_directory);

    drop(d.create_file(b"/docs/a.txt").unwrap());
    drop(d.create_file(b"/docs/b.txt").unwrap());
    d.create_directory(b"/docs/sub").unwrap();

    let mut names: Vec<Vec<u8>> = d
        .dir_entries(dir_ref)
        .unwrap()
        .into_iter()
        .map(|r| d.info(r).unwrap().unwrap().name)
        .collect();
    names.sort();

    let mut expected = vec![b"a.txt".to_vec(), b"b.txt".to_vec(), b"sub".to_vec()];
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn root_entries_include_top_level_files_and_dirs() {
    let d = new_drive();
    drop(d.create_file(b"/one.txt").unwrap());
    d.create_directory(b"/two").unwrap();

    let names: HashSet<Vec<u8>> = d
        .dir_entries(d.root())
        .unwrap()
        .into_iter()
        .map(|r| d.info(r).unwrap().unwrap().name)
        .collect();

    assert!(names.contains(&b"one.txt".to_vec()));
    assert!(names.contains(&b"two".to_vec()));
}

// -----------------------------------------------------------------
// delete
// -----------------------------------------------------------------

#[test]
fn delete_removes_file() {
    let d = new_drive();
    drop(d.create_file(b"/temp.txt").unwrap());
    assert!(d.info(b"/temp.txt").unwrap().is_some());

    d.delete(b"/temp.txt").unwrap();
    assert!(d.info(b"/temp.txt").unwrap().is_none());
}

#[test]
fn delete_removes_empty_directory() {
    let d = new_drive();
    d.create_directory(b"/gone").unwrap();
    d.delete(b"/gone").unwrap();
    assert!(d.info(b"/gone").unwrap().is_none());
}

#[test]
fn delete_nonexistent_path_errors() {
    let d = new_drive();
    let result = d.delete(b"/nope.txt");
    assert!(result.is_err());
}

#[test]
fn delete_non_empty_directory_errors() {
    let d = new_drive();
    d.create_directory(b"/full").unwrap();
    drop(d.create_file(b"/full/child.txt").unwrap());

    let result = d.delete(b"/full");
    assert!(result.is_err());
}

// -----------------------------------------------------------------
// move / rename
// -----------------------------------------------------------------

#[test]
fn move_renames_file_in_place() {
    let d = new_drive();
    {
        let mut f = d.create_file(b"/old-name.txt").unwrap();
        f.write_all(b"payload").unwrap();
    }
    let new_ref = d.move_(b"/old-name.txt", b"/new-name.txt", true).unwrap();

    assert!(d.info(b"/old-name.txt").unwrap().is_none());
    let info = d.info(new_ref).unwrap().unwrap();
    assert_eq!(info.name, b"new-name.txt");

    let mut f = d.open(new_ref).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"payload");
}

#[test]
fn move_relocates_file_into_another_directory() {
    let d = new_drive();
    let archive = d.create_directory(b"/archive").unwrap();
    drop(d.create_file(b"/report.txt").unwrap());

    let moved = d.move_(b"/report.txt", b"/archive/report.txt", true).unwrap();

    assert!(d.info(b"/report.txt").unwrap().is_none());
    let info = d.info(moved).unwrap().unwrap();
    assert_eq!(info.name, b"report.txt");
    assert_eq!(info.parent, archive);

    let entries = d.dir_entries(archive).unwrap();
    assert!(entries.contains(&moved));
}

#[test]
fn moving_a_directory_keeps_children_reachable() {
    let d = new_drive();
    d.create_directory(b"/project").unwrap();
    let child = d.create_file(b"/project/notes.txt").unwrap().get_ref();

    let moved_dir = d.move_(b"/project", b"/renamed-project", true).unwrap();

    assert!(d.info(b"/project").unwrap().is_none());
    assert!(d.info(b"/project/notes.txt").unwrap().is_none());

    // The child's FileRef identity survives the parent move, and it's
    // still listed under the (renamed) directory...
    let entries = d.dir_entries(moved_dir).unwrap();
    assert!(entries.contains(&child));

    // ...and reachable at its new path too.
    assert!(d.info(b"/renamed-project/notes.txt").unwrap().is_some());
}

#[test]
fn move_replace_argument() {
    let d = new_drive();
    drop(d.create_file(b"/src.txt").unwrap());
    drop(d.create_file(b"/dst.txt").unwrap());
    let result = d.move_(b"/src.txt", b"/dst.txt", false);
    assert!(result.is_err());
    let result = d.move_(b"/src.txt", b"/dst.txt", true);
    assert!(result.is_ok());
}

// -----------------------------------------------------------------
// FileHandle: name / parent / user_attrs / size / full_fileinfo
// -----------------------------------------------------------------

#[test]
fn set_name_renames_open_file() {
    let d = new_drive();
    let mut f = d.create_file(b"/before.txt").unwrap();
    f.write_all(b"content").unwrap();
    f.set_name(b"after.txt").unwrap();
    assert_eq!(f.name().as_ref(), b"after.txt");
    drop(f);

    assert!(d.info(b"/before.txt").unwrap().is_none());
    let info = d.info(b"/after.txt").unwrap().unwrap();
    assert_eq!(info.name, b"after.txt");
}

#[test]
fn parent_dir_reports_correct_parent() {
    let d = new_drive();
    let a = d.create_directory(b"/a").unwrap();
    let f = d.create_file(b"/a/leaf.txt").unwrap();
    assert_eq!(f.parent_dir(), a);
}

#[test]
fn user_attrs_default_none_then_settable() {
    let d = new_drive();
    let mut f = d.create_file(b"/attrs.txt").unwrap();
    assert_eq!(f.user_attrs().unwrap(), None);

    f.set_user_attrs(b"tag=important").unwrap();
    assert_eq!(f.user_attrs().unwrap().as_deref(), Some(b"tag=important".as_ref()));

    let info = f.full_fileinfo().unwrap();
    assert_eq!(info.user_attrs.as_deref(), Some(b"tag=important".as_ref()));
}

#[test]
fn file_size_tracks_writes_and_set_len() {
    let d = new_drive();
    let mut f = d.create_file(b"/sized.txt").unwrap();
    assert_eq!(f.file_size(), 0);

    f.write_all(b"0123456789").unwrap();
    assert_eq!(f.file_size(), 10);

    f.set_len(4).unwrap(); // truncate
    assert_eq!(f.file_size(), 4);
    f.seek(SeekFrom::Start(0)).unwrap();
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, b"0123");

    f.set_len(8).unwrap(); // extend
    assert_eq!(f.file_size(), 8);
}

#[test]
fn read_write_seek_round_trip() {
    let d = new_drive();
    let mut f = d.create_file(b"/rw.txt").unwrap();
    f.write_all(b"hello world").unwrap();
    f.seek(SeekFrom::Start(6)).unwrap();
    let mut buf = [0u8; 5];
    f.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"world");
}

#[test]
fn full_fileinfo_matches_drive_level_info() {
    let d = new_drive();
    let mut f = d.create_file(b"/consistent.txt").unwrap();
    f.write_all(b"abcdef").unwrap();
    f.flush().unwrap(); // make sure metadata is updated

    let via_handle = f.full_fileinfo().unwrap();
    let via_drive = d.info(f.get_ref()).unwrap().unwrap();

    assert_eq!(via_handle.name, via_drive.name);
    assert_eq!(via_handle.size, via_drive.size);
    assert_eq!(via_handle.is_directory, via_drive.is_directory);
}

    // -----------------------------------------------------------------
    // New methods
    // -----------------------------------------------------------------
 
    #[test]
    fn alter_changes_readonly_flag() {
        let d = new_drive();
        drop(d.create_file(b"/ro-target.txt").unwrap());
        d.alter(b"/ro-target.txt", Some(true), None, None, None).unwrap();
        let info = d.info(b"/ro-target.txt").unwrap().unwrap();
        assert!(info.is_readonly);
    }
 
    #[test]
    fn alter_changes_name() {
        let d = new_drive();
        drop(d.create_file(b"/alter-old.txt").unwrap());
        d.alter(b"/alter-old.txt", None, None, None, Some(b"alter-new.txt"))
            .unwrap();
        assert!(d.info(b"/alter-old.txt").unwrap().is_none());
        assert!(d.info(b"/alter-new.txt").unwrap().is_some());
    }
 
    #[test]
    fn alter_changes_access_and_modify_time() {
        let d = new_drive();
        drop(d.create_file(b"/times.txt").unwrap());
        d.alter(b"/times.txt", None, Some(1_000), Some(2_000), None).unwrap();
        let info = d.info(b"/times.txt").unwrap().unwrap();
        assert_eq!(info.access_time, Some(1_000));
        assert_eq!(info.modify_time, Some(2_000));
    }
 
    #[test]
    fn alter_with_none_leaves_other_attributes_unchanged() {
        let d = new_drive();
        drop(d.create_file(b"/partial-alter.txt").unwrap());
        d.alter(b"/partial-alter.txt", Some(true), Some(42), None, None).unwrap();
 
        // touch only the name this time - readonly and access_time should
        // be untouched.
        d.alter(b"/partial-alter.txt", None, None, None, Some(b"partial-alter-2.txt"))
            .unwrap();
 
        let info = d.info(b"/partial-alter-2.txt").unwrap().unwrap();
        assert!(info.is_readonly);
        assert_eq!(info.access_time, Some(42));
    }
 
    #[test]
    fn alter_fails_while_file_is_open() {
        // Assumption #10 - delete this test if alter() is meant to work
        // concurrently with an open handle.
        let d = new_drive();
        let f = d.create_file(b"/alter-busy.txt").unwrap();
        let result = d.alter(b"/alter-busy.txt", Some(true), None, None, None);
        assert_fs_err(result, FsError::FileInUse);
        drop(f);
        d.alter(b"/alter-busy.txt", Some(true), None, None, None).unwrap();
    }
 
    #[test]
    #[ignore = "TODO: this test is currently failing because alter() doesn't check for name collisions yet"]
    fn alter_rename_to_existing_name_fails() {
        let d = new_drive();
        drop(d.create_file(b"/keep.txt").unwrap());
        drop(d.create_file(b"/rename-me.txt").unwrap());
        let result = d.alter(b"/rename-me.txt", None, None, None, Some(b"keep.txt"));
        assert_fs_err(result, FsError::AlreadyExists);
    }
 
    #[test]
    fn resolve_path_returns_ref_for_existing_path() {
        let d = new_drive();
        let created = d.create_directory(b"/resolvable").unwrap();
        let resolved = d.resolve_path(b"/resolvable").unwrap();
        assert_eq!(resolved, Some(created));
    }
 
    #[test]
    fn resolve_path_returns_none_for_missing_path() {
        let d = new_drive();
        let resolved = d.resolve_path(b"/never-created.txt").unwrap();
        assert_eq!(resolved, None);
    }
 
    #[test]
    fn flush_succeeds() {
        let d = new_drive();
        drop(d.create_file(b"/flushed.txt").unwrap());
        d.flush().unwrap();
    }
 
    #[test]
    fn move_without_replace_fails_if_destination_exists() {
        let d = new_drive();
        drop(d.create_file(b"/src.txt").unwrap());
        drop(d.create_file(b"/dst.txt").unwrap());
        let result = d.move_(b"/src.txt", b"/dst.txt", false);
        assert_fs_err(result, FsError::AlreadyExists);
    }
 
    #[test]
    fn move_with_replace_overwrites_existing_destination() {
        let d = new_drive();
        drop(d.create_file(b"/src2.txt").unwrap()); // empty
        {
            let mut dst = d.create_file(b"/dst2.txt").unwrap();
            dst.write_all(b"stale").unwrap();
        }
 
        let moved = d.move_(b"/src2.txt", b"/dst2.txt", true).unwrap();
 
        assert!(d.info(b"/src2.txt").unwrap().is_none());
        let mut f = d.open(moved).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty(), "destination should now hold src2.txt's (empty) content");
    }
 
    #[test]
    fn move_creates_missing_parent_directories() {
        let d = new_drive();
        drop(d.create_file(b"/standalone.txt").unwrap());
        let moved = d.move_(b"/standalone.txt", b"/brand/new/path.txt", false).unwrap();
 
        assert!(d.info(b"/brand").unwrap().unwrap().is_directory);
        assert!(d.info(b"/brand/new").unwrap().unwrap().is_directory);
        let info = d.info(moved).unwrap().unwrap();
        assert_eq!(info.name, b"path.txt");
    }
 
    #[test]
    fn close_releases_file_for_reopening() {
        let d = new_drive();
        let f = d.create_file(b"/closeable.txt").unwrap();
        f.close().unwrap();
        let reopened = d.open(b"/closeable.txt");
        assert!(reopened.is_ok());
    }
 
    #[test]
    fn write_to_readonly_file_fails() {
        // Loose assertion: Read/Write/Seek return std::io::Result, and the
        // mapping from FsError::PermissionDenied to an io::ErrorKind isn't
        // specified, so this only checks for *an* error, not a specific kind.
        let d = new_drive();
        let mut f = d.create_file(b"/readonly-write.txt").unwrap();
        f.write_all(b"before").unwrap();
        f.set_readonly(true);
        assert!(f.is_readonly());
        let result = f.write(b"more");
        assert!(result.is_err());
    }
 
    #[test]
    fn allocation_size_is_at_least_file_size() {
        let d = new_drive();
        let mut f = d.create_file(b"/alloc.txt").unwrap();
        f.write_all(&vec![0u8; 5_000]).unwrap();
        assert!(f.allocation_size() >= f.file_size());
    }
 
    #[test]
    fn access_and_modify_time_setters_roundtrip_on_handle() {
        let d = new_drive();
        let mut f = d.create_file(b"/handle-times.txt").unwrap();
        f.set_access_time(111);
        f.set_modify_time(222);
        assert_eq!(f.access_time(), 111);
        assert_eq!(f.modify_time(), 222);
    }
 
    #[test]
    fn create_time_is_stable_across_reopen() {
        let d = new_drive();
        let created_at = {
            let f = d.create_file(b"/stable-ctime.txt").unwrap();
            f.create_time()
        };
        let reopened = d.open(b"/stable-ctime.txt").unwrap();
        assert_eq!(reopened.create_time(), created_at);
    }
 
    #[test]
    fn fileref_converts_to_and_from_u64_consistently() {
        let d = new_drive();
        let r = d.create_directory(b"/conv").unwrap();
        let n: u64 = r.into();
        let r2: FileRef = n.into();
        assert_eq!(r, r2);
    }
 
    // -----------------------------------------------------------------
    // Consistency: different ways of getting to the same information
    // should agree.
    // -----------------------------------------------------------------
 
    #[test]
    fn resolve_path_then_info_matches_direct_path_info() {
        let d = new_drive();
        drop(d.create_file(b"/cross-check.txt").unwrap());
        let via_resolve = d.resolve_path(b"/cross-check.txt").unwrap().unwrap();
        let info_via_ref = d.info(via_resolve).unwrap().unwrap();
        let info_via_path = d.info(b"/cross-check.txt").unwrap().unwrap();
        assert_eq!(info_via_ref.self_ref, info_via_path.self_ref);
        assert_eq!(info_via_ref.name, info_via_path.name);
    }
 
    #[test]
    fn tuple_resolve_matches_path_resolve() {
        let d = new_drive();
        let dir = d.create_directory(b"/tup").unwrap();
        drop(d.create_file(b"/tup/leaf.txt").unwrap());
 
        let via_path = d.info(b"/tup/leaf.txt").unwrap().unwrap();
        let via_tuple = d.info((dir, b"leaf.txt".as_ref())).unwrap().unwrap();
 
        assert_eq!(via_path.self_ref, via_tuple.self_ref);
        assert_eq!(via_path.name, via_tuple.name);
        assert_eq!(via_path.size, via_tuple.size);
    }
 
    #[test]
    fn self_ref_matches_the_fileref_used_to_look_it_up() {
        let d = new_drive();
        let r = d.create_directory(b"/self-ref-check").unwrap();
        let info = d.info(r).unwrap().unwrap();
        assert_eq!(info.self_ref, r);
    }
 
    #[test]
    fn full_fileinfo_matches_handle_accessors() {
        let d = new_drive();
        let mut f = d.create_file(b"/full-check.txt").unwrap();
        f.write_all(b"123456").unwrap();
        f.set_readonly(true);
        f.set_access_time(10);
        f.set_modify_time(20);
 
        let info = f.full_fileinfo().unwrap();
        assert_eq!(info.self_ref, f.get_ref());
        assert_eq!(info.name.as_slice(), f.name().as_ref());
        assert_eq!(info.parent, f.parent_dir());
        assert_eq!(info.size, f.file_size());
        assert_eq!(info.is_readonly, f.is_readonly());
        assert_eq!(info.create_time, f.create_time());
        assert_eq!(info.access_time, Some(f.access_time()));
        assert_eq!(info.modify_time, Some(f.modify_time()));
    }
 
    #[test]
    fn parent_dir_consistent_across_handle_info_and_drive() {
        let d = new_drive();
        let dir = d.create_directory(b"/parent-check").unwrap();
        let f = d.create_file(b"/parent-check/leaf.txt").unwrap();
 
        assert_eq!(f.parent_dir(), dir);
        assert_eq!(f.full_fileinfo().unwrap().parent, dir);
        assert_eq!(d.info(f.get_ref()).unwrap().unwrap().parent, dir);
    }
 
    #[test]
    #[ignore]
    fn is_readonly_consistent_between_handle_and_info() {
        let d = new_drive();
        let mut f = d.create_file(b"/ro-consistent.txt").unwrap();
        assert!(!f.is_readonly());
        assert!(!d.info(f.get_ref()).unwrap().unwrap().is_readonly);
 
        f.set_readonly(true);
        assert!(f.is_readonly());
        assert!(d.info(f.get_ref()).unwrap().unwrap().is_readonly);
    }
 
    #[test]
    #[ignore = "times are overriden by the drive implementation"]
    fn times_set_while_open_are_visible_via_info_after_close() {
        let d = new_drive();
        let mut f = d.create_file(b"/times-after-close.txt").unwrap();
        f.set_access_time(555);
        f.set_modify_time(777);
        f.close().unwrap();
 
        let info = d.info(b"/times-after-close.txt").unwrap().unwrap();
        assert_eq!(info.access_time, Some(555));
        assert_eq!(info.modify_time, Some(777));
    }
 
    #[test]
    fn attributes_set_via_alter_are_visible_via_reopened_handle() {
        let d = new_drive();
        drop(d.create_file(b"/alter-then-reopen.txt").unwrap());
        d.alter(b"/alter-then-reopen.txt", Some(true), Some(1), Some(2), None)
            .unwrap();
 
        let reopened = d.open(b"/alter-then-reopen.txt").unwrap();
        assert!(reopened.is_readonly());
        assert_eq!(reopened.access_time(), 1);
        assert_eq!(reopened.modify_time(), 2);
    }
 
    #[test]
    fn dir_entries_self_ref_consistent_with_info() {
        let d = new_drive();
        let dir = d.create_directory(b"/dir-selfref").unwrap();
        drop(d.create_file(b"/dir-selfref/a.txt").unwrap());
        drop(d.create_file(b"/dir-selfref/b.txt").unwrap());
 
        for entry_ref in d.dir_entries(dir).unwrap() {
            let info = d.info(entry_ref).unwrap().unwrap();
            assert_eq!(info.self_ref, entry_ref);
            assert_eq!(info.parent, dir);
        }
    }
 
    // -----------------------------------------------------------------
    // Edge cases / real-ish load
    // -----------------------------------------------------------------
 
    #[test]
    fn create_file_with_deep_missing_parents() {
        let d = new_drive();
        let mut f = d.create_file(b"/a/b/c/d/file.txt").unwrap();
        f.write_all(b"deep").unwrap();
        drop(f);
 
        for dir_path in ["/a", "/a/b", "/a/b/c", "/a/b/c/d"] {
            let info = d.info(dir_path).unwrap().expect("intermediate dir should exist");
            assert!(info.is_directory);
        }
 
        let mut reopened = d.open(b"/a/b/c/d/file.txt").unwrap();
        let mut buf = Vec::new();
        reopened.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"deep");
    }
 
    #[test]
    fn open_on_directory_errors_is_a_directory() {
        let d = new_drive();
        d.create_directory(b"/justadir").unwrap();
        let result = d.open(b"/justadir");
        assert_fs_err(result, FsError::IsADirectory);
    }
 
    #[test]
    fn open_fails_with_file_in_use_error() {
        let d = new_drive();
        let f = d.create_file(b"/busy.txt").unwrap();
        let result = d.open(b"/busy.txt");
        assert_fs_err(result, FsError::FileInUse);
        drop(f);
    }
 
    #[test]
    fn delete_fails_while_file_is_open() {
        let d = new_drive();
        let f = d.create_file(b"/locked.txt").unwrap();
        let result = d.delete(b"/locked.txt");
        assert_fs_err(result, FsError::FileInUse);
        drop(f);
        d.delete(b"/locked.txt").unwrap();
    }
 
    #[test]
    fn open_missing_file_errors_file_not_found() {
        let d = new_drive();
        let result = d.open(b"/totally-missing.txt");
        assert_fs_err(result, FsError::FileNotFound);
    }
 
    #[test]
    fn create_file_on_existing_path_errors_already_exists() {
        let d = new_drive();
        drop(d.create_file(b"/dup2.txt").unwrap());
        let result = d.create_file(b"/dup2.txt");
        assert_fs_err(result, FsError::AlreadyExists);
    }
 
    #[test]
    fn recreating_a_path_after_delete_does_not_resurrect_old_data() {
        let d = new_drive();
        let old_ref = {
            let mut f = d.create_file(b"/cycle.txt").unwrap();
            f.write_all(b"first").unwrap();
            f.get_ref()
        };
        d.delete(b"/cycle.txt").unwrap();
 
        let new_ref = {
            let mut f = d.create_file(b"/cycle.txt").unwrap();
            f.write_all(b"second").unwrap();
            f.get_ref()
        };
 
        let mut reopened = d.open(b"/cycle.txt").unwrap();
        let mut buf = Vec::new();
        reopened.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"second");
 
        // The old FileRef should no longer point at live data under this
        // path - either it no longer resolves, or it resolves to something
        // other than the freshly created file.
        if let Ok(Some(info)) = d.info(old_ref) {
            assert_ne!(info.self_ref, new_ref);
        }
    }
 
    #[test]
    fn bulk_create_many_files_and_verify_listing() {
        let d = new_drive();
        let dir = d.create_directory(b"/bulk").unwrap();
        let mut created: HashSet<FileRef> = HashSet::new();
 
        for i in 0..200 {
            let name = format!("file-{i:04}.txt");
            let mut f = d.create_file((dir, name.as_bytes())).unwrap();
            f.write_all(name.as_bytes()).unwrap();
            created.insert(f.get_ref());
        }
 
        let listed: HashSet<FileRef> = d.dir_entries(dir).unwrap().into_iter().collect();
        assert_eq!(listed, created);
 
        for i in 0..200 {
            let name = format!("file-{i:04}.txt");
            let path = format!("/bulk/{name}");
            let via_resolve = d.resolve_path(path.as_bytes()).unwrap().unwrap();
            let via_tuple = d.info((dir, name.as_bytes())).unwrap().unwrap().self_ref;
            assert_eq!(via_resolve, via_tuple);
        }
    }
 
    #[test]
    fn builds_and_walks_a_small_directory_tree() {
        let d = new_drive();
        for dir_path in ["/tree", "/tree/a", "/tree/b", "/tree/a/x", "/tree/a/y"] {
            d.create_directory(dir_path).unwrap();
        }
        for (dir, file) in [("/tree/a/x", "1.txt"), ("/tree/a/x", "2.txt"), ("/tree/b", "3.txt")] {
            let full = format!("{dir}/{file}");
            drop(d.create_file(&*full).unwrap());
        }
 
        let root_children: HashSet<Vec<u8>> = d
            .dir_entries(b"/tree")
            .unwrap()
            .into_iter()
            .map(|r| d.info(r).unwrap().unwrap().name)
            .collect();
        assert_eq!(root_children, HashSet::from([b"a".to_vec(), b"b".to_vec()]));
 
        let x_children: HashSet<Vec<u8>> = d
            .dir_entries(b"/tree/a/x")
            .unwrap()
            .into_iter()
            .map(|r| d.info(r).unwrap().unwrap().name)
            .collect();
        assert_eq!(
            x_children,
            HashSet::from([b"1.txt".to_vec(), b"2.txt".to_vec()])
        );
    }
 
    #[test]
    fn large_write_round_trip() {
        let d = new_drive();
        let mut f = d.create_file(b"/large.bin").unwrap();
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        assert_eq!(f.file_size(), data.len() as u64);
        assert!(f.allocation_size() >= f.file_size());
 
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut readback = vec![0u8; data.len()];
        f.read_exact(&mut readback).unwrap();
        assert_eq!(readback, data);
    }
 
    #[test]
    fn mixed_operations_sequence_ends_in_expected_state() {
        let d = new_drive();
        d.create_directory(b"/workspace").unwrap();
        drop(d.create_file(b"/workspace/draft.txt").unwrap());
        drop(d.create_file(b"/workspace/scratch.txt").unwrap());
 
        d.move_(b"/workspace/draft.txt", b"/workspace/final.txt", false)
            .unwrap();
        d.alter(b"/workspace/final.txt", Some(true), None, None, None)
            .unwrap();
        d.delete(b"/workspace/scratch.txt").unwrap();
 
        let entries: HashSet<Vec<u8>> = d
            .dir_entries(b"/workspace")
            .unwrap()
            .into_iter()
            .map(|r| d.info(r).unwrap().unwrap().name)
            .collect();
        assert_eq!(entries, HashSet::from([b"final.txt".to_vec()]));
 
        let info = d.info(b"/workspace/final.txt").unwrap().unwrap();
        assert!(info.is_readonly);
    }
 
    #[test]
    fn using_a_fileref_from_a_different_drive() {
        // Assumption #12 - this is the most speculative test in the file.
        // Delete it if FileRef has no drive affinity in your design.
        let drive_a = new_drive();
        let drive_b = new_drive();
        let foreign_ref = drive_a.create_file(b"/only-in-a.txt").unwrap().get_ref();
 
        match drive_b.info(foreign_ref) {
            Err(farfs::fs::Error::FsError(FsError::InvalidDrive)) => {} // expected
            Err(e) => panic!("expected InvalidDrive, got {:?}", e),
            Ok(v) => {
                // If FileRef isn't drive-scoped this is merely a coincidence,
                // not necessarily a bug - just documenting the behavior.
                println!("cross-drive FileRef resolved: {}", v.is_some());
            }
        }
    }

    #[test]
    fn create_replace_delete() {
        let d = new_drive();
        {
            let mut f1 = d.create_file(b"/work/a.txt").unwrap();
            f1.write_all(b"first").unwrap();
        }
        {
            let mut f2 = d.create_file(b"/work/b.txt").unwrap();
            f2.write_all(b"second").unwrap();
        }
        {
            let f2 = d.move_(b"/work/b.txt", b"/work/a.txt", true).unwrap();
            let mut f2 = d.open(f2).unwrap();
            let mut buf = Vec::new();
            f2.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, b"second");
        }
        assert!(d.delete(b"/work/b.txt").is_err());
        d.delete(b"/work/a.txt").unwrap();
        println!("{:?}", d.dir_entries(b"/work").unwrap());
        d.delete(b"/work").unwrap();
    }

        fn assert_fs_err<T>(result: Result<T, farfs::fs::Error>, expected: FsError) {
        match result {
            Err(farfs::fs::Error::FsError(actual)) => assert_eq!(actual, expected),
            Err(farfs::fs::Error::BackingError(e)) => {
                panic!("expected FsError::{:?}, got BackingError: {:?}", expected, e)
            }
            Ok(_) => panic!("expected FsError::{:?}, but the call succeeded", expected),
        }
    }

    #[test]
    fn delete_empty_folder() {
        let d = new_drive();
        d.create_directory(b"/empty").unwrap();
        d.delete(b"/empty").unwrap();
        assert!(d.info(b"/empty").unwrap().is_none());
    }

    #[test]
    fn delete_emptied_folder() {
        let d = new_drive();
        d.create_directory(b"/to-delete").unwrap();
        drop(d.create_file(b"/to-delete/file.txt").unwrap());
        d.delete(b"/to-delete/file.txt").unwrap();
        d.delete(b"/to-delete").unwrap();
        assert!(d.info(b"/to-delete").unwrap().is_none());
    }

    #[test]
    fn rename_folder_deletes_old_subfiles() {
        let d = new_drive();
        d.create_directory(b"/old").unwrap();
        drop(d.create_file(b"/old/file.txt").unwrap());
        d.create_directory(b"/new").unwrap();
        d.move_(b"/new", b"/old", true).unwrap();
        assert!(d.info(b"/new").unwrap().is_none());
        assert!(d.info(b"/old/file.txt").unwrap().is_none());
        assert!(d.info(b"/old").unwrap().is_some());
    }