// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: Copyright © the fdio contributors

use bitflags::bitflags;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, StatxFlags, CWD};
use rustix::io::Errno;
use std::ffi::{CStr, CString};
use std::io;
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use crate::chase::Chase;
use crate::engine::ChaseFlags;
use crate::steps::{ChaseStepMkdir, ChaseStepPath};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct LookupFlags: u32 {
        const NOFOLLOW            = 1 << 0;
        const NO_AUTOMOUNT        = 1 << 1;
        const RESOLVE_NO_SYMLINKS = 1 << 2;
        const NO_DISPLAY_PATH     = 1 << 3;
    }
}

impl LookupFlags {
    fn track_path(self) -> bool {
        !self.contains(LookupFlags::NO_DISPLAY_PATH)
    }

    fn to_chase_flags(self) -> ChaseFlags {
        let mut f = ChaseFlags::empty();
        if self.contains(LookupFlags::NOFOLLOW) {
            f |= ChaseFlags::NOFOLLOW;
        }
        if self.contains(LookupFlags::NO_AUTOMOUNT) {
            f |= ChaseFlags::NO_AUTOMOUNT;
        }
        if self.contains(LookupFlags::RESOLVE_NO_SYMLINKS) {
            f |= ChaseFlags::RESOLVE_NO_SYMLINKS;
        }
        f
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ResolveFlags: u32 {
        const NO_AUTOMOUNT        = 1 << 1;
        const RESOLVE_NO_SYMLINKS = 1 << 2;
        const NO_DISPLAY_PATH     = 1 << 3;
    }
}

impl ResolveFlags {
    fn track_path(self) -> bool {
        !self.contains(ResolveFlags::NO_DISPLAY_PATH)
    }

    fn to_chase_flags(self) -> ChaseFlags {
        let mut f = ChaseFlags::empty();
        if self.contains(ResolveFlags::NO_AUTOMOUNT) {
            f |= ChaseFlags::NO_AUTOMOUNT;
        }
        if self.contains(ResolveFlags::RESOLVE_NO_SYMLINKS) {
            f |= ChaseFlags::RESOLVE_NO_SYMLINKS;
        }
        f
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Dir;

#[derive(Clone, Copy, Debug)]
pub struct Regular;

#[derive(Clone, Copy, Debug)]
pub struct Socket;

#[derive(Clone, Copy, Debug)]
pub struct Symlink;

#[derive(Clone, Copy, Debug)]
pub struct Any;

pub(crate) fn fd_path(fd: BorrowedFd<'_>) -> Result<CString, Errno> {
    let proc_path = CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    fs::readlinkat(CWD, &proc_path, Vec::new())
}

pub(crate) fn proc_self_fd(fd: BorrowedFd<'_>) -> CString {
    CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveMode {
    InRoot,
    Beneath,
    Unconfined,
}

impl ResolveMode {
    fn to_chase_flags(self) -> ChaseFlags {
        match self {
            ResolveMode::InRoot => ChaseFlags::RESOLVE_IN_ROOT,
            ResolveMode::Beneath => ChaseFlags::RESOLVE_BENEATH,
            ResolveMode::Unconfined => ChaseFlags::empty(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RootData {
    pub(crate) fd: OwnedFd,
    pub(crate) mode: ResolveMode,
    pub(crate) path: CString,
}

#[derive(Debug)]
pub struct FileRef<T = Any> {
    fd: OwnedFd,
    root: Arc<RootData>,
    is_root: bool,
    display_path: Option<CString>,
    _marker: PhantomData<T>,
}

impl<T> FileRef<T> {
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    pub fn display_path(&self) -> Option<&CStr> {
        self.display_path.as_deref()
    }

    pub fn display_abs_path(&self) -> Option<CString> {
        let relative = self.display_path.as_ref()?;
        let root_bytes = self.root.path.to_bytes();
        let rel_bytes = relative.to_bytes();
        if rel_bytes == b"." {
            Some(self.root.path.clone())
        } else {
            let mut abs = Vec::with_capacity(root_bytes.len() + 1 + rel_bytes.len());
            abs.extend_from_slice(root_bytes);
            abs.push(b'/');
            abs.extend_from_slice(rel_bytes);
            Some(CString::new(abs).unwrap())
        }
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        let st = fs::statx(
            self.fd.as_fd(),
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::TYPE,
        )?;
        if !StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::TYPE) {
            return Err(Errno::OPNOTSUPP.into());
        }
        Ok(FileType::from_raw_mode(u32::from(st.stx_mode)))
    }

    pub(crate) fn into_type<U>(self) -> FileRef<U> {
        FileRef {
            fd: self.fd,
            root: self.root,
            is_root: self.is_root,
            display_path: self.display_path,
            _marker: PhantomData,
        }
    }

    fn do_resolve<U>(
        &self,
        path: &CStr,
        flags: ChaseFlags,
        mkdir: Option<Mode>,
        track_path: bool,
    ) -> io::Result<FileRef<U>> {
        let mut chase_flags = flags | self.root.mode.to_chase_flags();

        if mkdir.is_some() {
            chase_flags |= ChaseFlags::MUST_BE_DIRECTORY;
        }
        let is_child = !self.is_root && self.root.mode != ResolveMode::Unconfined;

        let effective_path_owned: Option<CString>;
        let effective_path: &CStr;
        let effective_dirfd: BorrowedFd<'_>;

        if is_child {
            let root_fd = self.root.fd.as_fd();
            let root_path = fd_path(root_fd)?;
            let child_path = fd_path(self.fd.as_fd())?;

            let root_bytes = root_path.to_bytes();
            let child_bytes = child_path.to_bytes();
            let path_bytes = path.to_bytes();

            if child_bytes == root_bytes {
                effective_path = path;
                effective_path_owned = None;
            } else if child_bytes.starts_with(root_bytes)
                && child_bytes.get(root_bytes.len()) == Some(&b'/')
            {
                let relative = &child_bytes[root_bytes.len() + 1..];
                let mut full = Vec::with_capacity(relative.len() + 1 + path_bytes.len());
                full.extend_from_slice(relative);
                full.push(b'/');
                full.extend_from_slice(path_bytes);
                effective_path_owned =
                    Some(CString::new(full).map_err(|_| io::Error::from(Errno::INVAL))?);
                effective_path = effective_path_owned.as_ref().unwrap();
            } else {
                return Err(Errno::XDEV.into());
            }

            effective_dirfd = root_fd;
        } else {
            effective_path = path;
            effective_path_owned = None;
            effective_dirfd = self.fd.as_fd();
        }
        let _ = effective_path_owned;

        let mut mkdir_step = mkdir.map(ChaseStepMkdir::new);
        let mut path_tracker = if track_path {
            Some(ChaseStepPath::new(chase_flags))
        } else {
            None
        };

        let mut chase = Chase::new(&effective_dirfd, chase_flags);
        if let Some(ref mut s) = mkdir_step {
            chase = chase.step(s);
        }
        if let Some(ref mut s) = path_tracker {
            chase = chase.step(s);
        }

        let result = chase.resolve(effective_path)?;
        let display_path = result.get::<ChaseStepPath>().cloned();

        Ok(FileRef {
            fd: result.into_fd(),
            root: self.root.clone(),
            is_root: false,
            display_path,
            _marker: PhantomData,
        })
    }
}

impl<T> std::fmt::Display for FileRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.display_abs_path() {
            Some(abs) => f.write_str(abs.to_str().map_err(|_| std::fmt::Error)?),
            None => write!(f, "<fd:{}>", self.fd.as_raw_fd()),
        }
    }
}

impl FileRef<Any> {
    pub fn try_into_dir(self) -> Result<FileRef<Dir>, Self> {
        match self.file_type() {
            Ok(FileType::Directory) => Ok(self.into_type()),
            _ => Err(self),
        }
    }

    pub fn try_into_regular(self) -> Result<FileRef<Regular>, Self> {
        match self.file_type() {
            Ok(FileType::RegularFile) => Ok(self.into_type()),
            _ => Err(self),
        }
    }

    pub fn try_into_socket(self) -> Result<FileRef<Socket>, Self> {
        match self.file_type() {
            Ok(FileType::Socket) => Ok(self.into_type()),
            _ => Err(self),
        }
    }

    pub fn try_into_symlink(self) -> Result<FileRef<Symlink>, Self> {
        match self.file_type() {
            Ok(FileType::Symlink) => Ok(self.into_type()),
            _ => Err(self),
        }
    }
}

impl FileRef<Dir> {
    pub fn resolve(&self, path: &CStr, flags: LookupFlags) -> io::Result<FileRef> {
        self.do_resolve(path, flags.to_chase_flags(), None, flags.track_path())
    }

    pub fn resolve_dir(&self, path: &CStr, flags: ResolveFlags) -> io::Result<FileRef<Dir>> {
        self.do_resolve(
            path,
            flags.to_chase_flags() | ChaseFlags::MUST_BE_DIRECTORY,
            None,
            flags.track_path(),
        )
    }

    pub fn resolve_regular(
        &self,
        path: &CStr,
        flags: ResolveFlags,
    ) -> io::Result<FileRef<Regular>> {
        self.do_resolve(
            path,
            flags.to_chase_flags() | ChaseFlags::MUST_BE_REGULAR,
            None,
            flags.track_path(),
        )
    }

    pub fn resolve_socket(&self, path: &CStr, flags: ResolveFlags) -> io::Result<FileRef<Socket>> {
        self.do_resolve(
            path,
            flags.to_chase_flags() | ChaseFlags::MUST_BE_SOCKET,
            None,
            flags.track_path(),
        )
    }

    pub fn resolve_symlink(
        &self,
        path: &CStr,
        flags: ResolveFlags,
    ) -> io::Result<FileRef<Symlink>> {
        self.do_resolve(
            path,
            flags.to_chase_flags() | ChaseFlags::NOFOLLOW | ChaseFlags::MUST_BE_SYMLINK,
            None,
            flags.track_path(),
        )
    }

    pub fn resolve_mkdir(
        &self,
        path: &CStr,
        mode: Mode,
        flags: ResolveFlags,
    ) -> io::Result<FileRef<Dir>> {
        self.do_resolve(path, flags.to_chase_flags(), Some(mode), flags.track_path())
    }
}

impl<T> AsFd for FileRef<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

pub struct Root {
    data: Arc<RootData>,
    file: FileRef<Dir>,
}

impl Root {
    pub fn open(path: &CStr, mode: ResolveMode) -> io::Result<Self> {
        let fd = fs::openat(
            CWD,
            path,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        let abs_path = fd_path(fd.as_fd())?;
        let fd2 = rustix::io::fcntl_dupfd_cloexec(&fd, 0)?;
        let data = Arc::new(RootData {
            fd,
            mode,
            path: abs_path,
        });
        let file = FileRef {
            fd: fd2,
            root: data.clone(),
            is_root: true,
            display_path: None,
            _marker: PhantomData,
        };
        Ok(Self { data, file })
    }

    pub fn mode(&self) -> ResolveMode {
        self.data.mode
    }
}

impl Deref for Root {
    type Target = FileRef<Dir>;
    fn deref(&self) -> &FileRef<Dir> {
        &self.file
    }
}

impl AsFd for Root {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{AtFlags, StatxFlags};
    use std::os::unix::io::OwnedFd;

    fn get_ino(fd: BorrowedFd<'_>) -> u64 {
        fs::statx(
            fd,
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::INO,
        )
        .unwrap()
        .stx_ino
    }

    fn symlinkat(target: &CStr, dirfd: BorrowedFd<'_>, linkpath: &CStr) {
        fs::symlinkat(target, dirfd, linkpath).unwrap();
    }

    fn openat_path(dfd: BorrowedFd<'_>, path: &CStr) -> OwnedFd {
        fs::openat(
            dfd,
            path,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap()
    }

    fn open_tmpdir(tmp: &tempfile::TempDir) -> Root {
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        Root::open(&path, ResolveMode::Beneath).unwrap()
    }

    fn open_tmpdir_root(tmp: &tempfile::TempDir, mode: ResolveMode) -> Root {
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        Root::open(&path, mode).unwrap()
    }

    fn mkdir_p(tmp: &tempfile::TempDir, rel: &str) -> OwnedFd {
        std::fs::create_dir_all(tmp.path().join(rel)).unwrap();
        let tfd = open_tmpdir(tmp);
        openat_path(tfd.as_fd(), &CString::new(rel).unwrap())
    }

    #[test]
    fn test_resolve_mkdir() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        mkdir_p(&tmp, "d1");

        let r = tfd
            .resolve_mkdir(
                c"d1/d2/d3",
                Mode::from_raw_mode(0o755),
                ResolveFlags::empty(),
            )
            .unwrap();
        let made_ino = get_ino(r.as_fd());

        let verify = openat_path(tfd.as_fd(), c"d1/d2/d3");
        assert_eq!(get_ino(verify.as_fd()), made_ino);
    }

    fn check_display_path(
        dfd: &FileRef<Dir>,
        path: &CStr,
        flags: ChaseFlags,
        expected_ino: u64,
        expected_path: &CStr,
    ) {
        let flags = flags | ChaseFlags::DEBUG_NO_OPENAT2;

        let r: FileRef = dfd.do_resolve(path, flags, None, true).unwrap();
        assert_eq!(get_ino(r.as_fd()), expected_ino);
        assert_eq!(r.display_path().unwrap(), expected_path);

        let r: FileRef = dfd
            .do_resolve(path, flags | ChaseFlags::DEBUG_NO_OPEN_TREE, None, true)
            .unwrap();
        assert_eq!(get_ino(r.as_fd()), expected_ino);
        assert_eq!(r.display_path().unwrap(), expected_path);
    }

    #[test]
    fn test_display_path_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let baz = mkdir_p(&tmp, "a/b/c");
        let baz_ino = get_ino(baz.as_fd());
        let a_ino = get_ino(openat_path(tfd.as_fd(), c"a").as_fd());

        check_display_path(&tfd, c"a/b/c", ChaseFlags::empty(), baz_ino, c"a/b/c");
        check_display_path(&tfd, c"a/./b/c", ChaseFlags::empty(), baz_ino, c"a/b/c");
        check_display_path(&tfd, c"a/b/c/", ChaseFlags::empty(), baz_ino, c"a/b/c");
        check_display_path(&tfd, c"a/b/../b/c", ChaseFlags::empty(), baz_ino, c"a/b/c");
        check_display_path(&tfd, c"a////b/c", ChaseFlags::empty(), baz_ino, c"a/b/c");
        check_display_path(&tfd, c"a/b/c/../..", ChaseFlags::empty(), a_ino, c"a");
    }

    #[test]
    fn test_display_path_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let target = mkdir_p(&tmp, "real/dir");
        let target_ino = get_ino(target.as_fd());

        symlinkat(c"real/dir", tfd.as_fd(), c"link");
        let link_ino = get_ino(openat_path(tfd.as_fd(), c"link").as_fd());

        check_display_path(&tfd, c"link", ChaseFlags::empty(), target_ino, c"real/dir");
        check_display_path(&tfd, c"link", ChaseFlags::NOFOLLOW, link_ino, c"link");
    }

    #[test]
    fn test_display_path_symlink_in_middle() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        mkdir_p(&tmp, "real/sub");
        let sub = openat_path(tfd.as_fd(), c"real/sub");
        let sub_ino = get_ino(sub.as_fd());

        symlinkat(c"real", tfd.as_fd(), c"link");

        check_display_path(&tfd, c"link/sub", ChaseFlags::empty(), sub_ino, c"real/sub");
    }

    #[test]
    fn test_display_path_absolute_symlink_in_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir_root(&tmp, ResolveMode::InRoot);
        mkdir_p(&tmp, "target");
        let target_ino = get_ino(openat_path(tfd.as_fd(), c"target").as_fd());

        symlinkat(c"/target", tfd.as_fd(), c"link");

        check_display_path(&tfd, c"link", ChaseFlags::empty(), target_ino, c"target");
    }

    #[test]
    fn test_display_path_dot() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let tfd_ino = get_ino(tfd.as_fd());

        check_display_path(&tfd, c".", ChaseFlags::empty(), tfd_ino, c".");
    }

    #[test]
    fn test_resolve_mkdir_with_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        mkdir_p(&tmp, "d1");

        let r: FileRef<Dir> = tfd
            .do_resolve(
                c"d1/d2/d3",
                ChaseFlags::DEBUG_NO_OPENAT2,
                Some(Mode::from_raw_mode(0o755)),
                true,
            )
            .unwrap();
        let made_ino = get_ino(r.as_fd());

        let verify = openat_path(tfd.as_fd(), c"d1/d2/d3");
        assert_eq!(get_ino(verify.as_fd()), made_ino);
        assert_eq!(r.display_path().unwrap(), c"d1/d2/d3");
    }

    #[test]
    fn test_child_in_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_tmpdir_root(&tmp, ResolveMode::InRoot);
        let root_target = mkdir_p(&tmp, "target");
        let root_target_ino = get_ino(root_target.as_fd());
        mkdir_p(&tmp, "sub/target");
        let sub_target_ino = get_ino(openat_path(root.as_fd(), c"sub/target").as_fd());
        assert_ne!(root_target_ino, sub_target_ino);

        symlinkat(
            c"/target",
            openat_path(root.as_fd(), c"sub").as_fd(),
            c"link",
        );

        let child: FileRef<Dir> = root
            .do_resolve(
                c"sub",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();

        let r: FileRef = child
            .do_resolve(c"link", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap();
        assert_eq!(get_ino(r.as_fd()), root_target_ino);
    }

    #[test]
    fn test_child_beneath() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_tmpdir_root(&tmp, ResolveMode::Beneath);
        mkdir_p(&tmp, "a");
        let sibling = mkdir_p(&tmp, "sibling");
        let sibling_ino = get_ino(sibling.as_fd());

        let child: FileRef<Dir> = root
            .do_resolve(
                c"a",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();

        let r: FileRef = child
            .do_resolve(c"../sibling", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap();
        assert_eq!(get_ino(r.as_fd()), sibling_ino);

        let err = child
            .do_resolve::<Any>(c"../..", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::XDEV.raw_os_error()));
    }

    #[test]
    fn test_grandchild() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_tmpdir_root(&tmp, ResolveMode::InRoot);
        let root_target = mkdir_p(&tmp, "target");
        let root_target_ino = get_ino(root_target.as_fd());
        mkdir_p(&tmp, "a/target");
        mkdir_p(&tmp, "a/b/target");

        symlinkat(
            c"/target",
            openat_path(root.as_fd(), c"a/b").as_fd(),
            c"link",
        );

        let child: FileRef<Dir> = root
            .do_resolve(
                c"a",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();
        let grandchild: FileRef<Dir> = child
            .do_resolve(
                c"b",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();

        let r: FileRef = grandchild
            .do_resolve(c"link", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap();
        assert_eq!(get_ino(r.as_fd()), root_target_ino);
    }

    #[test]
    fn test_child_renamed_within_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = open_tmpdir_root(&tmp, ResolveMode::InRoot);
        mkdir_p(&tmp, "orig/target");
        let target_ino = get_ino(openat_path(root.as_fd(), c"orig/target").as_fd());

        let child: FileRef<Dir> = root
            .do_resolve(
                c"orig",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();

        std::fs::rename(tmp.path().join("orig"), tmp.path().join("moved")).unwrap();

        let r: FileRef = child
            .do_resolve(c"target", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap();
        assert_eq!(get_ino(r.as_fd()), target_ino);
    }

    #[test]
    fn test_child_moved_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("root")).unwrap();
        std::fs::create_dir(tmp.path().join("outside")).unwrap();

        let root_path = CString::new(tmp.path().join("root").to_str().unwrap()).unwrap();
        let root = Root::open(&root_path, ResolveMode::InRoot).unwrap();
        mkdir_p(&tmp, "root/sub");

        let child: FileRef<Dir> = root
            .do_resolve(
                c"sub",
                ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::MUST_BE_DIRECTORY,
                None,
                false,
            )
            .unwrap();

        std::fs::rename(tmp.path().join("root/sub"), tmp.path().join("outside/sub")).unwrap();

        let err = child
            .do_resolve::<Any>(c".", ChaseFlags::DEBUG_NO_OPENAT2, None, false)
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(Errno::XDEV.raw_os_error()));
    }
}
