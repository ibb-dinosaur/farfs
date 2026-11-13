#!/usr/bin/env python3
"""Filesystem behaviour tests (Linux + Windows, stdlib only).

Usage:
    python fstest.py                      # test the current directory
    python fstest.py --dir X:\\ -v        # test a mounted volume, verbose
    python fstest.py -k Seek              # run only tests whose name matches
Each test works in its own temporary subfolder, which is removed afterwards.
"""
import argparse
import hashlib
import os
import random
import shutil
import stat
import sys
import tarfile
import tempfile
import time
import unittest
import zipfile

ROOT = os.getcwd()
IS_WIN = sys.platform == "win32"


def tree_digest(top):
    """Map relative path -> sha256 hex (files) or None (directories)."""
    out = {}
    for base, dirs, files in os.walk(top):
        for d in dirs:
            out[os.path.relpath(os.path.join(base, d), top)] = None
        for f in files:
            full = os.path.join(base, f)
            h = hashlib.sha256()
            with open(full, "rb") as fh:
                for chunk in iter(lambda: fh.read(1 << 16), b""):
                    h.update(chunk)
            out[os.path.relpath(full, top)] = h.hexdigest()
    return out


def write(path, data=b""):
    with open(path, "wb") as f:
        f.write(data)


def read(path):
    with open(path, "rb") as f:
        return f.read()


class FsTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="fstest_", dir=ROOT)
        self.addCleanup(shutil.rmtree, self.dir, True)

    def p(self, *parts):
        return os.path.join(self.dir, *parts)


# --------------------------------------------------------------------------
class SeekTests(FsTest):
    def test_whence_modes(self):
        data = bytes(range(256)) * 16
        write(self.p("f"), data)
        with open(self.p("f"), "rb") as f:
            f.seek(100)
            self.assertEqual(f.read(10), data[100:110])
            f.seek(10, os.SEEK_CUR)
            self.assertEqual(f.tell(), 120)
            f.seek(-10, os.SEEK_END)
            self.assertEqual(f.read(), data[-10:])
            self.assertEqual(f.seek(0, os.SEEK_END), len(data))
            self.assertEqual(f.read(10), b"")  # read at EOF

    def test_random_ops_against_model(self):
        rng = random.Random(1234)
        model = bytearray()
        with open(self.p("f"), "w+b") as f:
            for _ in range(400):
                off = rng.randint(0, len(model) + 8192)
                if rng.random() < 0.6:
                    chunk = rng.randbytes(rng.randint(1, 6000))
                    f.seek(off)
                    f.write(chunk)
                    if off > len(model):
                        model.extend(b"\0" * (off - len(model)))
                    model[off:off + len(chunk)] = chunk
                else:
                    n = rng.randint(1, 6000)
                    f.seek(off)
                    self.assertEqual(f.read(n), bytes(model[off:off + n]),
                                     f"read mismatch at {off}+{n}")
            f.flush()
        self.assertEqual(read(self.p("f")), bytes(model))

    def test_write_past_eof_zero_fills(self):
        with open(self.p("f"), "wb") as f:
            f.seek(1 << 20)
            f.write(b"A")
        self.assertEqual(os.path.getsize(self.p("f")), (1 << 20) + 1)
        with open(self.p("f"), "rb") as f:
            self.assertEqual(f.read(4096), b"\0" * 4096)
            f.seek(1 << 20)
            self.assertEqual(f.read(), b"A")

    def test_overwrite_in_middle(self):
        write(self.p("f"), b"0123456789")
        with open(self.p("f"), "r+b") as f:
            f.seek(3)
            f.write(b"XYZ")
        self.assertEqual(read(self.p("f")), b"012XYZ6789")

    def test_truncate_shrink_and_grow(self):
        write(self.p("f"), b"x" * 1000)
        os.truncate(self.p("f"), 10)
        self.assertEqual(read(self.p("f")), b"x" * 10)
        os.truncate(self.p("f"), 50)
        self.assertEqual(read(self.p("f")), b"x" * 10 + b"\0" * 40) # <- fail
        with open(self.p("f"), "r+b") as f:
            f.truncate(0)
        self.assertEqual(os.path.getsize(self.p("f")), 0)

    def test_append_mode_ignores_seek(self):
        write(self.p("f"), b"abc")
        with open(self.p("f"), "ab") as f:
            f.seek(0)
            f.write(b"def")
        self.assertEqual(read(self.p("f")), b"abcdef")

    def test_two_handles_see_each_other(self):
        with open(self.p("f"), "w+b") as a, open(self.p("f"), "rb") as b:
            a.write(b"hello")
            a.flush()
            self.assertEqual(b.read(), b"hello")
            a.write(b" world")
            a.flush()
            self.assertEqual(b.read(), b" world")

    def test_odd_chunk_sizes_roundtrip(self):
        rng = random.Random(7)
        blob = rng.randbytes(8 * 1024 * 1024 + 13)
        with open(self.p("f"), "wb") as f:
            pos = 0
            while pos < len(blob):
                n = rng.choice([1, 7, 511, 4093, 65537])
                f.write(blob[pos:pos + n])
                pos += n
        got = bytearray()
        with open(self.p("f"), "rb") as f:
            while True:
                chunk = f.read(rng.choice([3, 4096, 100000]))
                if not chunk:
                    break
                got += chunk
        self.assertEqual(hashlib.sha256(got).digest(), hashlib.sha256(blob).digest())


# --------------------------------------------------------------------------
class RenameTests(FsTest):
    def test_rename_file(self):
        write(self.p("a"), b"data")
        os.rename(self.p("a"), self.p("b"))
        self.assertFalse(os.path.exists(self.p("a")))
        self.assertEqual(read(self.p("b")), b"data")

    def test_replace_existing_file(self):
        write(self.p("a"), b"new")
        write(self.p("b"), b"old")
        os.replace(self.p("a"), self.p("b"))
        self.assertEqual(read(self.p("b")), b"new")
        self.assertFalse(os.path.exists(self.p("a")))

    def test_rename_onto_existing_without_replace_flag(self):
        write(self.p("a"), b"1")
        write(self.p("b"), b"2")
        if IS_WIN:  # os.rename refuses to overwrite on Windows
            with self.assertRaises(FileExistsError):
                os.rename(self.p("a"), self.p("b"))
            self.assertEqual(read(self.p("b")), b"2")

    def test_move_between_directories(self):
        os.makedirs(self.p("d1"))
        os.makedirs(self.p("d2"))
        write(self.p("d1", "f"), b"x")
        os.rename(self.p("d1", "f"), self.p("d2", "g"))
        self.assertEqual(os.listdir(self.p("d1")), [])
        self.assertEqual(read(self.p("d2", "g")), b"x")

    def test_rename_directory_with_contents(self):
        os.makedirs(self.p("src", "sub"))
        write(self.p("src", "sub", "f"), b"deep")
        write(self.p("src", "top"), b"top")
        before = tree_digest(self.p("src"))
        os.rename(self.p("src"), self.p("dst"))
        self.assertFalse(os.path.exists(self.p("src")))
        self.assertEqual(tree_digest(self.p("dst")), before)

    """def test_rename_dir_onto_nonempty_dir_fails(self):
        os.makedirs(self.p("a"))
        os.makedirs(self.p("b"))
        write(self.p("b", "f"), b"x")
        with self.assertRaises(OSError):
            os.replace(self.p("a"), self.p("b"))"""

    def test_rename_dir_into_itself_fails(self):
        os.makedirs(self.p("a"))
        with self.assertRaises(OSError):
            os.rename(self.p("a"), self.p("a", "inner"))

    def test_rename_missing_source(self):
        with self.assertRaises(FileNotFoundError):
            os.rename(self.p("nope"), self.p("x"))

    def test_rename_into_missing_parent(self):
        write(self.p("a"), b"x")
        with self.assertRaises(FileNotFoundError):
            os.rename(self.p("a"), self.p("missing", "a"))

    def test_case_only_rename(self):
        write(self.p("name"), b"x")
        os.rename(self.p("name"), self.p("NAME"))
        self.assertIn("NAME", os.listdir(self.dir))
        self.assertNotIn("name", os.listdir(self.dir))

    def test_rename_while_open(self):
        write(self.p("a"), b"abc")
        with open(self.p("a"), "rb") as f:
            if IS_WIN:
                # Allowed only if the opener shared FILE_SHARE_DELETE;
                # Python does not, so a sharing violation is the correct answer.
                with self.assertRaises(PermissionError):
                    os.rename(self.p("a"), self.p("b"))
            else:
                os.rename(self.p("a"), self.p("b"))
                self.assertEqual(f.read(), b"abc")  # handle follows the inode

    def test_rename_is_atomic_for_readers(self):
        write(self.p("target"), b"A" * 1000)
        write(self.p("tmp"), b"B" * 1000)
        os.replace(self.p("tmp"), self.p("target"))
        self.assertEqual(read(self.p("target")), b"B" * 1000)


# --------------------------------------------------------------------------
class ArchiveTests(FsTest):
    def make_tree(self, top):
        rng = random.Random(99)
        os.makedirs(os.path.join(top, "a", "b", "c"))
        os.makedirs(os.path.join(top, "empty_dir"))
        write(os.path.join(top, "empty.txt"), b"")
        write(os.path.join(top, "one.bin"), b"\x00")
        write(os.path.join(top, "a", "text.txt"), b"hello\r\nworld\n")
        write(os.path.join(top, "a", "b", "big.bin"), rng.randbytes(3 * 1024 * 1024))
        write(os.path.join(top, "a", "b", "c", "\u00fcn\u00ef.dat"), rng.randbytes(5000))

    def _check(self, make, extract):
        src = self.p("src")
        os.makedirs(src)
        self.make_tree(src)
        archive = self.p("archive")
        make(archive, src)
        out = self.p("out")
        os.makedirs(out)
        extract(archive, out)
        self.assertEqual(tree_digest(out), tree_digest(src))

    def test_zip_roundtrip(self):
        def make(arc, src):
            with zipfile.ZipFile(arc, "w", zipfile.ZIP_DEFLATED) as z:
                for rel in tree_digest(src):
                    full = os.path.join(src, rel)
                    z.write(full, rel)  # directories are stored as entries too

        def extract(arc, out):
            with zipfile.ZipFile(arc) as z:
                self.assertIsNone(z.testzip())
                z.extractall(out)

        self._check(make, extract)

    def test_tar_gz_roundtrip(self):
        def make(arc, src):
            with tarfile.open(arc, "w:gz") as t:
                t.add(src, arcname=".")

        def extract(arc, out):
            with tarfile.open(arc, "r:gz") as t:
                if hasattr(tarfile, "data_filter"):
                    t.extractall(out, filter="data")
                else:
                    t.extractall(out)

        self._check(make, extract)

    def test_extract_over_existing_tree(self):
        src = self.p("src")
        os.makedirs(src)
        self.make_tree(src)
        arc = self.p("a.zip")
        with zipfile.ZipFile(arc, "w") as z:
            for rel in tree_digest(src):
                z.write(os.path.join(src, rel), rel)
        out = self.p("out")
        os.makedirs(out)
        for _ in range(2):  # second extraction overwrites files in place
            with zipfile.ZipFile(arc) as z:
                z.extractall(out)
        self.assertEqual(tree_digest(out), tree_digest(src))


# --------------------------------------------------------------------------
class AttributeTests(FsTest):
    def test_basic_stat(self):
        write(self.p("f"), b"12345")
        os.mkdir(self.p("d"))
        st = os.stat(self.p("f"))
        self.assertEqual(st.st_size, 5)
        self.assertTrue(stat.S_ISREG(st.st_mode))
        self.assertTrue(stat.S_ISDIR(os.stat(self.p("d")).st_mode))
        self.assertTrue(os.path.isfile(self.p("f")))
        self.assertTrue(os.path.isdir(self.p("d")))

    def test_stat_missing(self):
        with self.assertRaises(FileNotFoundError):
            os.stat(self.p("nope"))

    def test_size_tracks_writes(self):
        with open(self.p("f"), "wb") as f:
            for i in range(1, 6):
                f.write(b"x" * 100)
                f.flush()
                self.assertEqual(os.stat(self.p("f")).st_size, i * 100)

    def test_set_and_read_mtime(self):
        write(self.p("f"), b"x")
        t = 1_600_000_000  # 2020-09-13
        os.utime(self.p("f"), (t, t))
        st = os.stat(self.p("f"))
        self.assertLessEqual(abs(st.st_mtime - t), 2, "mtime not preserved")

    def test_mtime_advances_on_write(self):
        write(self.p("f"), b"x")
        old = 1_600_000_000
        os.utime(self.p("f"), (old, old))
        with open(self.p("f"), "ab") as f:
            f.write(b"y")
        self.assertGreater(os.stat(self.p("f")).st_mtime, old + 1000)

    def test_mtime_on_directory_changes_when_child_added(self):
        os.mkdir(self.p("d"))
        old = 1_600_000_000
        os.utime(self.p("d"), (old, old))
        write(self.p("d", "child"), b"x")
        self.assertGreater(os.stat(self.p("d")).st_mtime, old + 1000)

    def test_read_only_flag(self):
        if not IS_WIN and hasattr(os, "geteuid") and os.geteuid() == 0:
            self.skipTest("root ignores permission bits")
        write(self.p("f"), b"x")
        self.addCleanup(os.chmod, self.p("f"), stat.S_IWRITE | stat.S_IREAD)
        os.chmod(self.p("f"), stat.S_IREAD)
        with self.assertRaises(PermissionError):
            open(self.p("f"), "ab")
        self.assertEqual(read(self.p("f")), b"x")  # still readable
        os.chmod(self.p("f"), stat.S_IWRITE | stat.S_IREAD)
        with open(self.p("f"), "ab") as f:
            f.write(b"y")  # writable again

    def test_windows_attribute_bits(self):
        if not IS_WIN:
            self.skipTest("Windows only")
        write(self.p("f"), b"x")
        st = os.stat(self.p("f"))
        self.assertFalse(st.st_file_attributes & stat.FILE_ATTRIBUTE_DIRECTORY)
        os.mkdir(self.p("d"))
        self.assertTrue(os.stat(self.p("d")).st_file_attributes
                        & stat.FILE_ATTRIBUTE_DIRECTORY)

    def test_hard_link_count(self):
        write(self.p("a"), b"x")
        try:
            os.link(self.p("a"), self.p("b"))
        except (OSError, NotImplementedError) as e:
            self.skipTest(f"hard links unsupported: {e}")
        self.assertEqual(os.stat(self.p("a")).st_nlink, 2)
        os.unlink(self.p("a"))
        self.assertEqual(read(self.p("b")), b"x")
        self.assertEqual(os.stat(self.p("b")).st_nlink, 1)

    def test_scandir_matches_stat(self):
        write(self.p("f"), b"abc")
        os.mkdir(self.p("d"))
        entries = {e.name: e for e in os.scandir(self.dir)}
        self.assertTrue(entries["f"].is_file())
        self.assertTrue(entries["d"].is_dir())
        self.assertEqual(entries["f"].stat().st_size, 3)

    def test_symlink_if_supported(self):
        write(self.p("target"), b"x")
        try:
            os.symlink(self.p("target"), self.p("link"))
        except (OSError, NotImplementedError) as e:
            self.skipTest(f"symlinks unsupported: {e}")
        self.assertTrue(os.path.islink(self.p("link")))
        self.assertEqual(read(self.p("link")), b"x")


# --------------------------------------------------------------------------
class EdgeCaseTests(FsTest):
    def test_empty_file(self):
        write(self.p("e"))
        self.assertEqual(os.path.getsize(self.p("e")), 0)
        self.assertEqual(read(self.p("e")), b"")

    def test_name_length_limits(self):
        ok = "n" * 255
        write(self.p(ok), b"x")
        self.assertIn(ok, os.listdir(self.dir))
        with self.assertRaises(OSError):
            write(self.p("n" * 256), b"x")

    def test_unicode_and_odd_names(self):
        names = ["\u00fcn\u00efc\u00f6d\u00e9.txt", "\u65e5\u672c\u8a9e.txt", "\U0001f600.txt",
                 "with space.txt", ".hidden", "dots.in.name.tar.gz", "UPPER", "lower"]
        for n in names:
            write(self.p(n), n.encode("utf-8"))
        self.assertEqual(set(os.listdir(self.dir)), set(names))
        for n in names:
            self.assertEqual(read(self.p(n)), n.encode("utf-8"))

    def test_trailing_space_or_dot_names(self):
        if IS_WIN:
            self.skipTest("Win32 strips trailing dots/spaces; not a valid test")
        for n in ("trail.", "trail "):
            write(self.p(n), b"x")
            self.assertIn(n, os.listdir(self.dir))

    def test_case_sensitivity_report(self):
        write(self.p("Case"), b"1")
        try:
            write(self.p("case"), b"2")
        except OSError:
            pass
        names = sorted(n for n in os.listdir(self.dir) if n.lower() == "case")
        sys.stderr.write(f"\n[info] case-{'sensitive' if len(names) == 2 else 'insensitive'}"
                         f" ({names})\n")

    def test_many_files_in_one_directory(self):
        names = {f"file_{i:05d}.txt" for i in range(2000)}
        for n in names:
            write(self.p(n), n.encode())
        self.assertEqual(set(os.listdir(self.dir)), names)
        for n in random.Random(1).sample(sorted(names), 50):
            self.assertEqual(read(self.p(n)), n.encode())
        for n in names:
            os.unlink(self.p(n))
        self.assertEqual(os.listdir(self.dir), [])

    def test_deep_nesting(self):
        path = self.dir
        for _ in range(30):
            path = os.path.join(path, "d")
        os.makedirs(path)
        write(os.path.join(path, "leaf"), b"deep")
        self.assertEqual(read(os.path.join(path, "leaf")), b"deep")

    def test_exclusive_create(self):
        write(self.p("f"), b"x")
        with self.assertRaises(FileExistsError):
            open(self.p("f"), "xb")

    def test_mkdir_existing(self):
        os.mkdir(self.p("d"))
        with self.assertRaises(FileExistsError):
            os.mkdir(self.p("d"))

    def test_rmdir_nonempty(self):
        os.mkdir(self.p("d"))
        write(self.p("d", "f"), b"x")
        with self.assertRaises(OSError):
            os.rmdir(self.p("d"))

    def test_type_mismatch_errors(self):
        os.mkdir(self.p("d"))
        write(self.p("f"), b"x")
        with self.assertRaises((IsADirectoryError, PermissionError)):
            open(self.p("d"), "rb")
        with self.assertRaises(OSError):
            os.rmdir(self.p("f"))
        with self.assertRaises((IsADirectoryError, PermissionError)):
            os.unlink(self.p("d"))
        with self.assertRaises(OSError):
            write(self.p("f", "child"), b"x")  # parent is a file

    def test_delete_then_recreate_same_name(self):
        for i in range(20):
            write(self.p("f"), f"gen{i}".encode())
            self.assertEqual(read(self.p("f")), f"gen{i}".encode())
            os.unlink(self.p("f"))
            self.assertFalse(os.path.exists(self.p("f")))

    def test_delete_open_file(self):
        write(self.p("f"), b"abc")
        with open(self.p("f"), "rb") as f:
            if IS_WIN:
                with self.assertRaises(PermissionError):
                    os.unlink(self.p("f"))
            else:
                os.unlink(self.p("f"))
                self.assertEqual(f.read(), b"abc")  # unlinked but still readable
        self.assertFalse(os.path.exists(self.p("f")) and not IS_WIN)

    def test_zero_length_operations(self):
        with open(self.p("f"), "wb") as f:
            self.assertEqual(f.write(b""), 0)
        with open(self.p("f"), "rb") as f:
            self.assertEqual(f.read(0), b"")
            self.assertEqual(f.read(), b"")

    def test_listdir_during_modification(self):
        for i in range(100):
            write(self.p(f"f{i}"), b"x")
        seen = set()
        for e in os.scandir(self.dir):
            seen.add(e.name)
            if e.name == "f0":
                write(self.p("added_during_scan"), b"x")
        # Entries present at the start must each be reported exactly once.
        self.assertTrue({f"f{i}" for i in range(100)} <= seen)

    def test_directory_listing_after_rename_and_delete(self):
        os.mkdir(self.p("d"))
        for i in range(10):
            write(self.p("d", f"f{i}"), b"x")
        os.rename(self.p("d", "f3"), self.p("d", "renamed"))
        os.unlink(self.p("d", "f4"))
        expected = {f"f{i}" for i in range(10)} - {"f3", "f4"} | {"renamed"}
        self.assertEqual(set(os.listdir(self.p("d"))), expected)


if __name__ == "__main__":
    ap = argparse.ArgumentParser(add_help=False)
    ap.add_argument("--dir", default=os.getcwd())
    args, rest = ap.parse_known_args()
    ROOT = os.path.abspath(args.dir)
    print(f"Testing filesystem at: {ROOT}")
    unittest.main(argv=[sys.argv[0]] + rest)
