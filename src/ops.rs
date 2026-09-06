// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: Copyright © the fdio contributors

use rustix::fs::{
    self, AtFlags, FileType, FlockOperation, Mode, OFlags, StatxFlags, Timestamps, XattrFlags, CWD,
};
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};

use rustix::io::Errno;

use bitflags::bitflags;

use crate::fileref::{
    fd_path, proc_self_fd, Any, Dir, FileRef, LookupFlags, Regular, Socket, Symlink,
};
use std::marker::PhantomData;

fn random_tmp_name() -> CString {
    let mut buf = [0u8; 8];
    rustix::rand::getrandom(&mut buf, rustix::rand::GetRandomFlags::empty())
        .expect("getrandom failed");
    CString::new(format!(".tmpfile.{:016x}", u64::from_ne_bytes(buf))).unwrap()
}

pub type ReadDirItem<T> = (fs::DirEntry, FileRef<T>);

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ReadDirFlags: u32 {
        const NOFOLLOW = 1 << 0;
    }
}

#[repr(transparent)]
pub struct Name(CStr);

impl Name {
    pub fn new(s: &CStr) -> Option<&Name> {
        let bytes = s.to_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return None;
        }
        Some(unsafe { &*(s as *const CStr as *const Name) })
    }

    pub fn as_cstr(&self) -> &CStr {
        &self.0
    }
}

pub trait NotSymlink {}
impl NotSymlink for Dir {}
impl NotSymlink for Regular {}
impl NotSymlink for Socket {}

fn copy_fd(src: BorrowedFd<'_>, dst: BorrowedFd<'_>) -> Result<u64, Errno> {
    let mut copied: u64 = 0;

    loop {
        match fs::copy_file_range(src, None, dst, None, 0x7fff_f000) {
            Ok(0) => return Ok(copied),
            Ok(n) => copied += n as u64,
            Err(Errno::NOSYS | Errno::XDEV | Errno::INVAL | Errno::OPNOTSUPP | Errno::OVERFLOW) => {
                break
            }
            Err(e) => return Err(e),
        }
    }

    loop {
        match fs::sendfile(dst, src, None, 0x7fff_f000) {
            Ok(0) => return Ok(copied),
            Ok(n) => copied += n as u64,
            Err(Errno::NOSYS | Errno::INVAL | Errno::OPNOTSUPP | Errno::OVERFLOW) => break,
            Err(e) => return Err(e),
        }
    }

    let mut buf = [0u8; 65536];
    loop {
        let n = rustix::io::read(src, &mut buf)?;
        if n == 0 {
            return Ok(copied);
        }
        let mut written = 0;
        while written < n {
            let w = rustix::io::write(dst, &buf[written..n])?;
            written += w;
        }
        copied += n as u64;
    }
}

trait ReadDirMode {
    type Item;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error>;
}

impl ReadDirMode for () {
    type Item = fs::DirEntry;
    fn produce(
        _: &FileRef<Dir>,
        entry: fs::DirEntry,
        _: ReadDirFlags,
    ) -> Result<Option<fs::DirEntry>, io::Error> {
        Ok(Some(entry))
    }
}

fn resolve_filtered<T>(
    file: &FileRef<Dir>,
    entry: fs::DirEntry,
    flags: ReadDirFlags,
    accepts: fn(FileType) -> bool,
) -> Result<Option<ReadDirItem<T>>, io::Error> {
    let dirent_type = entry.file_type();
    let follow = !flags.contains(ReadDirFlags::NOFOLLOW) && dirent_type == FileType::Symlink;

    if !accepts(dirent_type) && !follow {
        return Ok(None);
    }

    let lflags = if follow {
        LookupFlags::empty()
    } else {
        LookupFlags::NOFOLLOW
    };
    let file_ref: FileRef = file.resolve(entry.file_name(), lflags)?;

    if follow && !accepts(dirent_type) {
        match file_ref.file_type() {
            Ok(ft) if accepts(ft) => {}
            Ok(_) => return Ok(None),
            Err(e) => return Err(e),
        }
    }

    Ok(Some((entry, file_ref.into_type())))
}

impl ReadDirMode for Any {
    type Item = ReadDirItem<Any>;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error> {
        resolve_filtered(file, entry, flags, |_| true)
    }
}

impl ReadDirMode for Regular {
    type Item = ReadDirItem<Regular>;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error> {
        resolve_filtered(file, entry, flags, |ft| ft == FileType::RegularFile)
    }
}

impl ReadDirMode for Dir {
    type Item = ReadDirItem<Dir>;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error> {
        resolve_filtered(file, entry, flags, |ft| ft == FileType::Directory)
    }
}

impl ReadDirMode for Socket {
    type Item = ReadDirItem<Socket>;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error> {
        resolve_filtered(file, entry, flags, |ft| ft == FileType::Socket)
    }
}

impl ReadDirMode for Symlink {
    type Item = ReadDirItem<Symlink>;
    fn produce(
        file: &FileRef<Dir>,
        entry: fs::DirEntry,
        flags: ReadDirFlags,
    ) -> Result<Option<Self::Item>, io::Error> {
        resolve_filtered(file, entry, flags, |ft| ft == FileType::Symlink)
    }
}

struct ReadDir<'a, T: ReadDirMode = ()> {
    file: &'a FileRef<Dir>,
    dir: fs::Dir,
    flags: ReadDirFlags,
    _marker: PhantomData<T>,
}

impl<T: ReadDirMode> Iterator for ReadDir<'_, T> {
    type Item = io::Result<T::Item>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.dir.next()? {
                Ok(e) => e,
                Err(e) => return Some(Err(e.into())),
            };
            if entry.file_name().to_bytes() == b"." || entry.file_name().to_bytes() == b".." {
                continue;
            }
            match T::produce(self.file, entry, self.flags) {
                Ok(Some(item)) => return Some(Ok(item)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl<T> FileRef<T> {
    pub fn stat(&self, mask: StatxFlags) -> io::Result<rustix::fs::Statx> {
        let st = fs::statx(
            self.as_fd(),
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
            mask,
        )?;
        let filled = StatxFlags::from_bits_retain(st.stx_mask);
        if !filled.contains(mask) {
            return Err(io::Error::from(Errno::OPNOTSUPP));
        }
        Ok(st)
    }

    pub fn file_mode(&self) -> io::Result<Mode> {
        let st = self.stat(StatxFlags::MODE)?;
        Ok(Mode::from_raw_mode(u32::from(st.stx_mode)))
    }

    pub fn chown(
        &self,
        owner: Option<rustix::process::Uid>,
        group: Option<rustix::process::Gid>,
    ) -> io::Result<()> {
        Ok(fs::chownat(
            self.as_fd(),
            c"",
            owner,
            group,
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    pub fn set_times(&self, times: &Timestamps) -> io::Result<()> {
        Ok(fs::utimensat(
            self.as_fd(),
            c"",
            times,
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    pub fn getxattr(&self, name: &CStr) -> io::Result<Vec<u8>> {
        let path = fd_path(self.as_fd())?;
        let empty: &mut [u8; 0] = &mut [];
        let size = fs::lgetxattr(&path, name, empty)?;
        let mut buf = vec![0u8; size];
        let actual = fs::lgetxattr(&path, name, &mut buf)?;
        buf.truncate(actual);
        Ok(buf)
    }

    pub fn setxattr(&self, name: &CStr, value: &[u8], flags: XattrFlags) -> io::Result<()> {
        let path = fd_path(self.as_fd())?;
        Ok(fs::lsetxattr(&path, name, value, flags)?)
    }

    pub fn listxattr(&self) -> io::Result<Vec<u8>> {
        let path = fd_path(self.as_fd())?;
        let empty: &mut [u8; 0] = &mut [];
        let size = fs::llistxattr(&path, empty)?;
        let mut buf = vec![0u8; size];
        let actual = fs::llistxattr(&path, &mut buf)?;
        buf.truncate(actual);
        Ok(buf)
    }

    pub fn removexattr(&self, name: &CStr) -> io::Result<()> {
        let path = fd_path(self.as_fd())?;
        Ok(fs::lremovexattr(&path, name)?)
    }
}

impl<T: NotSymlink> FileRef<T> {
    pub fn reopen(&self, oflags: OFlags) -> io::Result<std::fs::File> {
        self.reopen_fd(oflags).map(std::fs::File::from)
    }

    pub fn reopen_fd(&self, oflags: OFlags) -> io::Result<OwnedFd> {
        assert!(!oflags.contains(OFlags::CREATE));
        assert!(!oflags.contains(OFlags::NOFOLLOW));
        let oflags = oflags | OFlags::CLOEXEC | OFlags::NOCTTY;
        Ok(fs::openat(
            CWD,
            proc_self_fd(self.as_fd()),
            oflags,
            Mode::empty(),
        )?)
    }

    pub fn chmod(&self, mode: Mode) -> io::Result<()> {
        Ok(fs::chmodat(
            CWD,
            proc_self_fd(self.as_fd()),
            mode,
            AtFlags::empty(),
        )?)
    }

    pub fn flock(&self, operation: FlockOperation) -> io::Result<OwnedFd> {
        let fd = self.reopen_fd(OFlags::RDONLY)?;
        fs::flock(&fd, operation)?;
        Ok(fd)
    }
}

impl FileRef<crate::fileref::Symlink> {
    pub fn read_link(&self) -> io::Result<CString> {
        Ok(fs::readlinkat(self.as_fd(), c"", Vec::new())?)
    }
}

impl FileRef<Regular> {
    pub fn read_file(&self) -> io::Result<Vec<u8>> {
        let fd = self.reopen_fd(OFlags::RDONLY)?;
        let st = self.stat(StatxFlags::SIZE)?;
        let mut buf = vec![0u8; st.stx_size as usize];
        let mut pos = 0;
        while pos < buf.len() {
            let n = rustix::io::read(&fd, &mut buf[pos..])?;
            if n == 0 {
                break;
            }
            pos += n;
        }
        buf.truncate(pos);
        Ok(buf)
    }

    pub fn copy_to<F: AsFd>(&self, dst_dir: &F, dst_name: &Name, mode: Mode) -> io::Result<()> {
        let src = self.reopen_fd(OFlags::RDONLY)?;
        let dst = fs::openat(
            dst_dir,
            c".",
            OFlags::WRONLY | OFlags::CLOEXEC | OFlags::TMPFILE,
            mode,
        )?;
        copy_fd(src.as_fd(), dst.as_fd())?;
        let tmp_cstr = random_tmp_name();
        fs::linkat(
            CWD,
            proc_self_fd(dst.as_fd()),
            dst_dir,
            &tmp_cstr,
            AtFlags::SYMLINK_FOLLOW,
        )?;
        match fs::renameat(dst_dir, &tmp_cstr, dst_dir, dst_name.as_cstr()) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::unlinkat(dst_dir, &tmp_cstr, AtFlags::empty());
                Err(e.into())
            }
        }
    }
}

impl FileRef<Dir> {
    fn open_dir_iter(&self) -> io::Result<fs::Dir> {
        let real_fd = fs::openat(
            self.as_fd(),
            c".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(fs::Dir::new(real_fd)?)
    }

    pub fn read_dir(&self) -> io::Result<impl Iterator<Item = io::Result<fs::DirEntry>> + '_> {
        Ok(ReadDir {
            file: self,
            dir: self.open_dir_iter()?,
            flags: ReadDirFlags::empty(),
            _marker: PhantomData::<()>,
        })
    }

    pub fn read_dir_any(
        &self,
        flags: ReadDirFlags,
    ) -> io::Result<impl Iterator<Item = io::Result<ReadDirItem<Any>>> + '_> {
        Ok(ReadDir {
            file: self,
            dir: self.open_dir_iter()?,
            flags,
            _marker: PhantomData::<Any>,
        })
    }

    pub fn read_dir_regular(
        &self,
        flags: ReadDirFlags,
    ) -> io::Result<impl Iterator<Item = io::Result<ReadDirItem<Regular>>> + '_> {
        Ok(ReadDir {
            file: self,
            dir: self.open_dir_iter()?,
            flags,
            _marker: PhantomData::<Regular>,
        })
    }

    pub fn read_dir_socket(
        &self,
        flags: ReadDirFlags,
    ) -> io::Result<impl Iterator<Item = io::Result<ReadDirItem<Socket>>> + '_> {
        Ok(ReadDir {
            file: self,
            dir: self.open_dir_iter()?,
            flags,
            _marker: PhantomData::<Socket>,
        })
    }

    pub fn unlink(&self, name: &Name) -> io::Result<()> {
        Ok(fs::unlinkat(
            self.as_fd(),
            name.as_cstr(),
            AtFlags::empty(),
        )?)
    }

    pub fn remove_dir(&self, name: &Name) -> io::Result<()> {
        Ok(fs::unlinkat(
            self.as_fd(),
            name.as_cstr(),
            AtFlags::REMOVEDIR,
        )?)
    }

    pub fn create_dir(&self, name: &Name, mode: Mode) -> io::Result<()> {
        Ok(fs::mkdirat(self.as_fd(), name.as_cstr(), mode)?)
    }

    pub fn ensure_dir(&self, name: &Name, mode: Mode) -> io::Result<()> {
        match fs::mkdirat(self.as_fd(), name.as_cstr(), mode) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => {
                let st = fs::statx(
                    self.as_fd(),
                    name.as_cstr(),
                    AtFlags::SYMLINK_NOFOLLOW,
                    StatxFlags::TYPE,
                )?;
                if FileType::from_raw_mode(u32::from(st.stx_mode)) != FileType::Directory {
                    return Err(Errno::EXIST.into());
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn create_symlink(&self, target: &CStr, name: &Name) -> io::Result<()> {
        Ok(fs::symlinkat(target, self.as_fd(), name.as_cstr())?)
    }

    pub fn rename<F: AsFd>(&self, old: &Name, new_dir: &F, new_name: &Name) -> io::Result<()> {
        Ok(fs::renameat(
            self.as_fd(),
            old.as_cstr(),
            new_dir,
            new_name.as_cstr(),
        )?)
    }

    pub fn link<F: AsFd>(&self, old: &Name, new_dir: &F, new_name: &Name) -> io::Result<()> {
        Ok(fs::linkat(
            self.as_fd(),
            old.as_cstr(),
            new_dir,
            new_name.as_cstr(),
            AtFlags::empty(),
        )?)
    }

    pub fn tmpfile(&self, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        Ok(fs::openat(
            self.as_fd(),
            c".",
            oflags | OFlags::TMPFILE | OFlags::CLOEXEC,
            mode,
        )?)
    }

    pub fn link_tmpfile(&self, tmpfd: BorrowedFd<'_>, name: &Name) -> io::Result<()> {
        Ok(fs::linkat(
            CWD,
            proc_self_fd(tmpfd),
            self.as_fd(),
            name.as_cstr(),
            AtFlags::SYMLINK_FOLLOW,
        )?)
    }

    pub fn replace_tmpfile(&self, tmpfd: BorrowedFd<'_>, name: &Name) -> io::Result<()> {
        let tmp_cstr = random_tmp_name();
        let tmp_name = Name::new(&tmp_cstr).unwrap();
        self.link_tmpfile(tmpfd, tmp_name)?;
        match fs::renameat(
            self.as_fd(),
            tmp_name.as_cstr(),
            self.as_fd(),
            name.as_cstr(),
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::unlinkat(self.as_fd(), tmp_name.as_cstr(), AtFlags::empty());
                Err(e.into())
            }
        }
    }

    pub fn write_file_atomic(&self, name: &Name, contents: &[u8], mode: Mode) -> io::Result<()> {
        let fd = self.tmpfile(OFlags::WRONLY, mode)?;
        let mut pos = 0;
        while pos < contents.len() {
            let n = rustix::io::write(&fd, &contents[pos..])?;
            pos += n;
        }
        self.replace_tmpfile(fd.as_fd(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileref::{ResolveFlags, ResolveMode, Root};

    fn n(s: &CStr) -> &Name {
        Name::new(s).unwrap()
    }

    fn open_tmpdir(tmp: &tempfile::TempDir) -> Root {
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        Root::open(&path, ResolveMode::Beneath).unwrap()
    }

    #[test]
    fn test_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        std::fs::write(tmp.path().join("hello"), b"world").unwrap();

        let file_ref = tfd
            .resolve_regular(c"hello", ResolveFlags::empty())
            .unwrap();

        let file = file_ref.reopen(OFlags::RDONLY).unwrap();
        use std::io::Read;
        let mut contents = String::new();
        (&file).read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "world");
    }

    #[test]
    fn test_reopen_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        let dir_fd = tfd.reopen_fd(OFlags::RDONLY).unwrap();
        let st = fs::statx(dir_fd.as_fd(), c"", AtFlags::EMPTY_PATH, StatxFlags::TYPE).unwrap();
        assert_eq!(
            FileType::from_raw_mode(u32::from(st.stx_mode)),
            FileType::Directory,
        );
    }

    #[test]
    fn test_stat() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        let st = tfd.stat(StatxFlags::TYPE).unwrap();
        assert_eq!(
            FileType::from_raw_mode(u32::from(st.stx_mode)),
            FileType::Directory,
        );
    }

    #[test]
    fn test_chmod() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("f"), b"").unwrap();

        let fref = tfd.resolve_regular(c"f", ResolveFlags::empty()).unwrap();

        fref.chmod(Mode::from_raw_mode(0o644)).unwrap();
        let st = fref.stat(StatxFlags::MODE).unwrap();
        assert_eq!(u32::from(st.stx_mode) & 0o777, 0o644);

        fref.chmod(Mode::from_raw_mode(0o600)).unwrap();
        let st = fref.stat(StatxFlags::MODE).unwrap();
        assert_eq!(u32::from(st.stx_mode) & 0o777, 0o600);
    }

    #[test]
    fn test_xattr_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("f"), b"").unwrap();

        let fref = tfd.resolve(c"f", LookupFlags::empty()).unwrap();

        fref.setxattr(c"user.test", b"hello", XattrFlags::CREATE)
            .unwrap();

        let val = fref.getxattr(c"user.test").unwrap();
        assert_eq!(val, b"hello");

        let list = fref.listxattr().unwrap();
        assert!(list.windows(b"user.test".len()).any(|w| w == b"user.test"));

        fref.removexattr(c"user.test").unwrap();
        assert_eq!(
            fref.getxattr(c"user.test").unwrap_err().raw_os_error(),
            Some(Errno::NODATA.raw_os_error()),
        );
    }

    #[test]
    fn test_dir_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        tfd.create_dir(n(c"sub"), Mode::from_raw_mode(0o755))
            .unwrap();
        assert!(tmp.path().join("sub").is_dir());

        tfd.ensure_dir(n(c"sub"), Mode::from_raw_mode(0o755))
            .unwrap();

        std::fs::write(tmp.path().join("sub/file"), b"").unwrap();
        let sub = tfd.resolve_dir(c"sub", ResolveFlags::empty()).unwrap();
        sub.unlink(n(c"file")).unwrap();
        assert!(!tmp.path().join("sub/file").exists());

        tfd.remove_dir(n(c"sub")).unwrap();
        assert!(!tmp.path().join("sub").exists());
    }

    #[test]
    fn test_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("old"), b"data").unwrap();

        tfd.rename(n(c"old"), &tfd, n(c"new")).unwrap();

        assert!(!tmp.path().join("old").exists());
        assert_eq!(std::fs::read(tmp.path().join("new")).unwrap(), b"data");
    }

    #[test]
    fn test_create_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        tfd.create_symlink(c"target", n(c"link")).unwrap();
        let target = fs::readlinkat(tfd.as_fd(), c"link", Vec::new()).unwrap();
        assert_eq!(target.as_bytes(), b"target");
    }

    #[test]
    fn test_tmpfile_and_link() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        let tmpf = tfd
            .tmpfile(OFlags::WRONLY, Mode::from_raw_mode(0o644))
            .unwrap();
        rustix::io::write(&tmpf, b"hello").unwrap();
        tfd.link_tmpfile(tmpf.as_fd(), n(c"linked")).unwrap();

        assert_eq!(std::fs::read(tmp.path().join("linked")).unwrap(), b"hello");
    }

    #[test]
    fn test_replace_tmpfile() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("existing"), b"old").unwrap();

        let tmpf = tfd
            .tmpfile(OFlags::WRONLY, Mode::from_raw_mode(0o644))
            .unwrap();
        rustix::io::write(&tmpf, b"new").unwrap();
        tfd.replace_tmpfile(tmpf.as_fd(), n(c"existing")).unwrap();

        assert_eq!(std::fs::read(tmp.path().join("existing")).unwrap(), b"new");
    }

    #[test]
    fn test_read_link() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        tfd.create_symlink(c"target", n(c"mylink")).unwrap();

        let link = tfd
            .resolve_symlink(c"mylink", ResolveFlags::empty())
            .unwrap();
        let target = link.read_link().unwrap();
        assert_eq!(target.as_bytes(), b"target");
    }

    #[test]
    fn test_link() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("original"), b"data").unwrap();

        tfd.link(n(c"original"), &tfd, n(c"hardlink")).unwrap();

        assert_eq!(std::fs::read(tmp.path().join("hardlink")).unwrap(), b"data");

        use std::os::unix::fs::MetadataExt;
        let orig_ino = std::fs::metadata(tmp.path().join("original"))
            .unwrap()
            .ino();
        let link_ino = std::fs::metadata(tmp.path().join("hardlink"))
            .unwrap()
            .ino();
        assert_eq!(orig_ino, link_ino);
    }

    #[test]
    fn test_flock() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("lockfile"), b"").unwrap();

        let fref = tfd
            .resolve_regular(c"lockfile", ResolveFlags::empty())
            .unwrap();

        let lock_fd = fref.flock(FlockOperation::LockExclusive).unwrap();
        drop(lock_fd);
    }

    #[test]
    fn test_read_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("hello"), b"world").unwrap();

        let file = tfd
            .resolve_regular(c"hello", ResolveFlags::empty())
            .unwrap();
        let contents = file.read_file().unwrap();
        assert_eq!(contents, b"world");
    }

    #[test]
    fn test_write_file_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);

        tfd.write_file_atomic(n(c"output"), b"atomic data", Mode::from_raw_mode(0o644))
            .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("output")).unwrap(),
            b"atomic data",
        );
    }

    #[test]
    fn test_write_file_atomic_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("output"), b"old").unwrap();

        tfd.write_file_atomic(n(c"output"), b"new", Mode::from_raw_mode(0o644))
            .unwrap();
        assert_eq!(std::fs::read(tmp.path().join("output")).unwrap(), b"new");
    }

    #[test]
    fn test_copy_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("src"), b"copy me").unwrap();

        let src = tfd.resolve_regular(c"src", ResolveFlags::empty()).unwrap();
        src.copy_to(&tfd, n(c"dst"), Mode::from_raw_mode(0o644))
            .unwrap();

        assert_eq!(std::fs::read(tmp.path().join("dst")).unwrap(), b"copy me");
        assert!(tmp.path().join("src").exists());
    }

    #[test]
    fn test_copy_file_large() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let data = vec![0x42u8; 256 * 1024];
        std::fs::write(tmp.path().join("big"), &data).unwrap();

        let src = tfd.resolve_regular(c"big", ResolveFlags::empty()).unwrap();
        src.copy_to(&tfd, n(c"big_copy"), Mode::from_raw_mode(0o644))
            .unwrap();
        assert_eq!(std::fs::read(tmp.path().join("big_copy")).unwrap(), data);
    }

    #[test]
    fn test_read_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("a"), b"").unwrap();
        std::fs::write(tmp.path().join("b"), b"").unwrap();
        tfd.create_dir(n(c"c"), Mode::from_raw_mode(0o755)).unwrap();

        let entries: Vec<_> = tfd.read_dir().unwrap().collect::<io::Result<_>>().unwrap();
        let mut names: Vec<Vec<u8>> = entries
            .iter()
            .map(|e| e.file_name().to_bytes().to_vec())
            .collect();
        names.sort();
        assert_eq!(names, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn test_read_dir_file_type() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        std::fs::write(tmp.path().join("file"), b"").unwrap();
        tfd.create_dir(n(c"dir"), Mode::from_raw_mode(0o755))
            .unwrap();
        tfd.create_symlink(c"file", n(c"link")).unwrap();

        let entries: Vec<_> = tfd
            .read_dir_any(ReadDirFlags::NOFOLLOW)
            .unwrap()
            .collect::<io::Result<_>>()
            .unwrap();
        assert_eq!(entries.len(), 3);
        for (entry, fref) in &entries {
            match entry.file_name().to_bytes() {
                b"file" => {
                    assert_eq!(entry.file_type(), FileType::RegularFile);
                    let st = fref.stat(StatxFlags::TYPE).unwrap();
                    assert_eq!(
                        FileType::from_raw_mode(u32::from(st.stx_mode)),
                        FileType::RegularFile,
                    );
                }
                b"dir" => assert_eq!(entry.file_type(), FileType::Directory),
                b"link" => assert_eq!(entry.file_type(), FileType::Symlink),
                _ => panic!("unexpected entry: {:?}", entry.file_name()),
            }
        }
    }

    #[test]
    fn test_root_ops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        let root = Root::open(&path, ResolveMode::InRoot).unwrap();

        let st = root.stat(StatxFlags::TYPE).unwrap();
        assert_eq!(
            FileType::from_raw_mode(u32::from(st.stx_mode)),
            FileType::Directory,
        );

        root.create_dir(n(c"testdir"), Mode::from_raw_mode(0o755))
            .unwrap();
        assert!(tmp.path().join("testdir").is_dir());
        root.remove_dir(n(c"testdir")).unwrap();
    }

    #[test]
    fn test_read_dir_follow_in_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        std::fs::write(tmp.path().join("target/file"), b"").unwrap();
        std::os::unix::fs::symlink("/target", tmp.path().join("link")).unwrap();

        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        let root = Root::open(&path, ResolveMode::InRoot).unwrap();

        let entries: Vec<_> = root
            .read_dir_any(ReadDirFlags::empty())
            .unwrap()
            .collect::<io::Result<_>>()
            .unwrap();

        let (entry, link_ref) = entries
            .into_iter()
            .find(|(e, _)| e.file_name().to_bytes() == b"link")
            .unwrap();
        assert_eq!(entry.file_type(), FileType::Symlink);

        let st = link_ref.stat(StatxFlags::TYPE).unwrap();
        assert_eq!(
            FileType::from_raw_mode(u32::from(st.stx_mode)),
            FileType::Directory,
        );

        let link_dir = link_ref.try_into_dir().unwrap();
        let child_entries: Vec<_> = link_dir
            .read_dir_any(ReadDirFlags::NOFOLLOW)
            .unwrap()
            .collect::<io::Result<_>>()
            .unwrap();
        assert_eq!(child_entries.len(), 1);
        assert_eq!(child_entries[0].0.file_name().to_bytes(), b"file");
    }
}
