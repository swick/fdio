// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: Copyright © the fdio contributors

// Implements the algorithm from https://gitlab.gnome.org/GNOME/libglnx/-/blob/master/glnx-chase.c

use bitflags::bitflags;
use rustix::fs::{self, openat2, AtFlags, FileType, Mode, OFlags, ResolveFlags, StatxFlags, CWD};
use rustix::io::Errno;
use rustix::mount::{open_tree, OpenTreeFlags};
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

pub type ChaseCallback<'a> =
    dyn FnMut(Result<OwnedFd, Errno>, BorrowedFd<'_>, &CStr) -> Result<OwnedFd, Errno> + 'a;

const AUTOFS_SUPER_MAGIC: i64 = 0x0187;
const CHASE_MAX: i32 = 32;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ChaseFlags: u32 {
        /// Disable triggering of automounts.
        const NO_AUTOMOUNT         = 1 << 1;
        /// Do not follow the path's right-most component. When the path's
        /// right-most component refers to a symlink, return an O_PATH fd of
        /// the symlink itself.
        const NOFOLLOW             = 1 << 2;
        /// Do not permit the path resolution to succeed if any component of
        /// the resolution is not a descendant of the directory indicated by
        /// dirfd.
        const RESOLVE_BENEATH      = 1 << 3;
        /// Symlinks are resolved relative to the given dirfd instead of root.
        const RESOLVE_IN_ROOT      = 1 << 4;
        /// Fail if any symlink is encountered.
        const RESOLVE_NO_SYMLINKS  = 1 << 5;
        /// Fail if the path's right-most component is not a regular file.
        const MUST_BE_REGULAR      = 1 << 6;
        /// Fail if the path's right-most component is not a directory.
        const MUST_BE_DIRECTORY    = 1 << 7;
        /// Fail if the path's right-most component is not a socket.
        const MUST_BE_SOCKET       = 1 << 8;
        /// Fail if the path's right-most component is not a symlink.
        const MUST_BE_SYMLINK      = 1 << 9;

        #[doc(hidden)]
        const DEBUG_NO_OPENAT2     = 1 << 31;
        #[doc(hidden)]
        const DEBUG_NO_OPEN_TREE   = 1 << 30;
    }
}

const DEBUG_FLAGS: ChaseFlags = ChaseFlags::DEBUG_NO_OPENAT2.union(ChaseFlags::DEBUG_NO_OPEN_TREE);
const MUST_BE_FLAGS: ChaseFlags = ChaseFlags::MUST_BE_REGULAR
    .union(ChaseFlags::MUST_BE_DIRECTORY)
    .union(ChaseFlags::MUST_BE_SOCKET)
    .union(ChaseFlags::MUST_BE_SYMLINK);
const RESOLVE_FLAGS: ChaseFlags = ChaseFlags::RESOLVE_BENEATH
    .union(ChaseFlags::RESOLVE_IN_ROOT)
    .union(ChaseFlags::RESOLVE_NO_SYMLINKS);

static CAN_OPEN_TREE: AtomicBool = AtomicBool::new(true);
static CAN_OPENAT2: AtomicBool = AtomicBool::new(true);

#[derive(Debug)]
pub struct ChaseError {
    errno: Errno,
    context: &'static str,
}

impl ChaseError {
    fn new(errno: Errno, context: &'static str) -> Self {
        Self { errno, context }
    }

    pub fn errno(&self) -> Errno {
        self.errno
    }

    pub fn context(&self) -> &'static str {
        self.context
    }
}

impl std::fmt::Display for ChaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let desc = io::Error::from(self.errno);
        if self.context.is_empty() {
            write!(f, "{desc}")
        } else {
            write!(f, "{}: {desc}", self.context)
        }
    }
}

impl std::error::Error for ChaseError {}

impl From<Errno> for ChaseError {
    fn from(errno: Errno) -> Self {
        Self { errno, context: "" }
    }
}

impl From<ChaseError> for io::Error {
    fn from(e: ChaseError) -> io::Error {
        io::Error::new(io::Error::from(e.errno).kind(), e)
    }
}

#[derive(Clone, Copy)]
struct StatxNode {
    file_type: FileType,
    dev_major: u32,
    dev_minor: u32,
    ino: u64,
    mnt_id: u64,
}

impl StatxNode {
    fn new(fd: BorrowedFd<'_>, at_flags: AtFlags) -> Result<Self, Errno> {
        // rustix does not expose STATX_MNT_ID_UNIQUE yet. We request it via
        // from_bits_retain() so that kernels which support it return the
        // non-recyclable 64-bit mount ID. On older kernels only STATX_MNT_ID
        // is returned, which is still fine for our same-walk comparisons.
        const STATX_MNT_ID_UNIQUE: u32 = 0x4000;

        let flags = AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW | at_flags;
        let mask = StatxFlags::TYPE
            | StatxFlags::INO
            | StatxFlags::MNT_ID
            | StatxFlags::from_bits_retain(STATX_MNT_ID_UNIQUE);

        let buf = fs::statx(fd, c"", flags, mask)?;

        let returned = StatxFlags::from_bits_retain(buf.stx_mask);
        if !returned.contains(StatxFlags::TYPE | StatxFlags::INO)
            || (returned & (StatxFlags::MNT_ID | StatxFlags::from_bits_retain(STATX_MNT_ID_UNIQUE)))
                .is_empty()
        {
            return Err(Errno::NODATA);
        }

        Ok(Self {
            file_type: FileType::from_raw_mode(u32::from(buf.stx_mode)),
            dev_major: buf.stx_dev_major,
            dev_minor: buf.stx_dev_minor,
            ino: buf.stx_ino,
            mnt_id: buf.stx_mnt_id,
        })
    }

    fn inode_same(&self, other: &Self) -> bool {
        self.file_type == other.file_type
            && self.dev_major == other.dev_major
            && self.dev_minor == other.dev_minor
            && self.ino == other.ino
    }

    fn mount_same(&self, other: &Self) -> bool {
        self.mnt_id == other.mnt_id
    }
}

#[derive(Clone, Copy)]
struct StatxFileType(FileType);

impl StatxFileType {
    fn new(fd: BorrowedFd<'_>, at_flags: AtFlags) -> Result<Self, Errno> {
        let flags = AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW | at_flags;
        let st = fs::statx(fd, c"", flags, StatxFlags::TYPE)?;

        if !StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::TYPE) {
            return Err(Errno::NODATA);
        }

        Ok(Self(FileType::from_raw_mode(u32::from(st.stx_mode))))
    }
}

impl PartialEq<FileType> for StatxFileType {
    fn eq(&self, other: &FileType) -> bool {
        self.0 == *other
    }
}

impl std::ops::Deref for StatxFileType {
    type Target = FileType;
    fn deref(&self) -> &FileType {
        &self.0
    }
}

struct Segment {
    name: CString,
    is_dot_dot: bool,
    is_last: bool,
}

struct RemainingPath {
    buffer: Vec<u8>,
    offset: usize,
}

impl RemainingPath {
    fn new(path: &[u8]) -> Self {
        Self {
            buffer: path.to_vec(),
            offset: 0,
        }
    }

    fn next_segment(&mut self) -> Result<Option<Segment>, Errno> {
        loop {
            while self.offset < self.buffer.len() && self.buffer[self.offset] == b'/' {
                self.offset += 1;
            }

            let seg_start = self.offset;
            while self.offset < self.buffer.len() && self.buffer[self.offset] != b'/' {
                self.offset += 1;
            }
            let seg_end = self.offset;
            let is_last = self.offset >= self.buffer.len();
            let seg_len = seg_end - seg_start;

            if seg_len == 0 || (seg_len == 1 && self.buffer[seg_start] == b'.') {
                if is_last {
                    return Ok(None);
                }
                continue;
            }

            let is_dot_dot = seg_len == 2
                && self.buffer[seg_start] == b'.'
                && self.buffer[seg_start + 1] == b'.';

            let name = CString::new(&self.buffer[seg_start..seg_end]).map_err(|_| Errno::INVAL)?;

            return Ok(Some(Segment {
                name,
                is_dot_dot,
                is_last,
            }));
        }
    }

    fn prepend_symlink(&mut self, link: &[u8]) {
        let remaining = &self.buffer[self.offset..];
        let mut new_buf = Vec::with_capacity(link.len() + 1 + remaining.len());
        new_buf.extend_from_slice(link);
        if !remaining.is_empty() {
            new_buf.push(b'/');
            new_buf.extend_from_slice(remaining);
        }
        self.buffer = new_buf;
        self.offset = 0;
    }
}

/// If `fd` points to an autofs mount point, trigger the automount and
/// re-open the path. Returns `fd` unchanged otherwise.
fn trigger_automount(
    dirfd: BorrowedFd<'_>,
    path: &CStr,
    fd: OwnedFd,
    oflags: OFlags,
) -> Result<OwnedFd, Errno> {
    // fstatfs can be used to determine if it is, in fact, an untriggered
    // automount point (f_type == AUTOFS_SUPER_MAGIC).
    let stfs = fs::fstatfs(&fd)?;
    if stfs.f_type != AUTOFS_SUPER_MAGIC {
        return Ok(fd);
    }

    // For some reason, openat with O_PATH | O_DIRECTORY does trigger
    // automounts, without us having to actually open the file, so let's use
    // this here. It only works for directories though.
    match fs::openat(fd.as_fd(), c".", oflags | OFlags::DIRECTORY, Mode::empty()) {
        Ok(fd) => return Ok(fd),
        Err(Errno::NOTDIR) => {}
        Err(e) => return Err(e),
    }

    // The automount is not a directory, so let's try to open the file, which
    // can fail because we are missing permissions, but that's okay, we only
    // need to trigger automount.
    drop(fs::openat(
        fd.as_fd(),
        c".",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    ));

    // And try again with O_PATH.
    let new_fd = fs::openat(dirfd, path, oflags, Mode::empty())?;

    // Bail if we didn't manage to trigger the automount.
    let stfs = fs::fstatfs(&new_fd)?;
    if stfs.f_type == AUTOFS_SUPER_MAGIC {
        return Err(Errno::OPNOTSUPP);
    }

    Ok(new_fd)
}

/// Open a path as O_PATH | O_CLOEXEC.
///
/// Prefers open_tree (which handles automount suppression natively) and falls
/// back to openat + manual automount triggering on older kernels.
fn open_path(dirfd: BorrowedFd<'_>, path: &CStr, flags: ChaseFlags) -> Result<OwnedFd, Errno> {
    debug_assert!(
        (flags - ChaseFlags::NO_AUTOMOUNT - ChaseFlags::NOFOLLOW - DEBUG_FLAGS).is_empty()
    );

    if CAN_OPEN_TREE.load(Ordering::Relaxed) && !flags.contains(ChaseFlags::DEBUG_NO_OPEN_TREE) {
        let mut otflags = OpenTreeFlags::OPEN_TREE_CLOEXEC;
        if flags.contains(ChaseFlags::NOFOLLOW) {
            otflags |= OpenTreeFlags::AT_SYMLINK_NOFOLLOW;
        }
        if flags.contains(ChaseFlags::NO_AUTOMOUNT) {
            otflags |= OpenTreeFlags::AT_NO_AUTOMOUNT;
        }

        match open_tree(dirfd, path, otflags) {
            Ok(fd) => return Ok(fd),
            Err(
                // If open_tree is not supported, or blocked (EPERM),
                // we fall back to openat.
                Errno::OPNOTSUPP
                | Errno::NOTTY
                | Errno::NOSYS
                | Errno::AFNOSUPPORT
                | Errno::PFNOSUPPORT
                | Errno::PROTONOSUPPORT
                | Errno::SOCKTNOSUPPORT
                | Errno::NOPROTOOPT
                | Errno::PERM,
            ) => {
                CAN_OPEN_TREE.store(false, Ordering::Relaxed);
            }
            Err(err) => return Err(err),
        }
    }

    let mut oflags = OFlags::CLOEXEC | OFlags::PATH;
    if flags.contains(ChaseFlags::NOFOLLOW) {
        oflags |= OFlags::NOFOLLOW;
    }

    let fd = fs::openat(dirfd, path, oflags, Mode::empty())?;

    // openat does not trigger automounts, so we have to manually do so
    // unless NO_AUTOMOUNT was specified.
    if flags.contains(ChaseFlags::NO_AUTOMOUNT) {
        Ok(fd)
    } else {
        trigger_automount(dirfd, path, fd, oflags)
    }
}

fn open_root(flags: ChaseFlags) -> Result<OwnedFd, Errno> {
    // NO_AUTOMOUNT should be fine here because automount must have been
    // triggered already for the root.
    open_path(
        CWD,
        c"/",
        (flags & DEBUG_FLAGS) | ChaseFlags::NO_AUTOMOUNT | ChaseFlags::NOFOLLOW,
    )
}

fn chase_manual(
    dirfd: BorrowedFd<'_>,
    path: &CStr,
    flags: ChaseFlags,
    mut callback: Option<&mut ChaseCallback<'_>>,
) -> Result<OwnedFd, ChaseError> {
    let path_bytes = path.to_bytes();
    let path_is_absolute = path_bytes.first() == Some(&b'/');

    // Take a shortcut if none of the resolve flags are set (they would
    // require work here), NO_AUTOMOUNT is set (open_path only triggers the
    // automount for the last component in some cases), and there is no
    // callback.
    if (flags & (ChaseFlags::NO_AUTOMOUNT | RESOLVE_FLAGS)) == ChaseFlags::NO_AUTOMOUNT
        && callback.is_none()
    {
        return Ok(open_path(
            dirfd,
            path,
            ChaseFlags::NO_AUTOMOUNT | (flags & (ChaseFlags::NOFOLLOW | DEBUG_FLAGS)),
        )?);
    }

    let no_automount = if flags.contains(ChaseFlags::NO_AUTOMOUNT) {
        AtFlags::NO_AUTOMOUNT
    } else {
        AtFlags::empty()
    };

    let _root_owner: Option<OwnedFd>;
    let root: BorrowedFd<'_>;

    if path_is_absolute && flags.contains(ChaseFlags::RESOLVE_BENEATH) {
        return Err(ChaseError::new(
            Errno::XDEV,
            "absolute path not allowed with RESOLVE_BENEATH",
        ));
    } else if !path_is_absolute || flags.contains(ChaseFlags::RESOLVE_IN_ROOT) {
        // The absolute path is relative to dirfd with RESOLVE_IN_ROOT, and
        // a relative path is always relative. In both cases we use dirfd as
        // our chase root.
        root = dirfd;
        _root_owner = None;
    } else {
        // For absolute paths, we ignore dirfd and use the actual root / for
        // our chase root.
        debug_assert!(path_is_absolute);
        _root_owner = Some(open_root(flags)?);
        root = _root_owner.as_ref().unwrap().as_fd();
    }

    // Add root to path_st, so we can verify if we get back to it.
    let st = StatxNode::new(root, no_automount)?;
    let mut path_st = vec![st];

    let mut remaining_path = RemainingPath::new(path_bytes);
    let mut current: Option<OwnedFd> = None;
    let mut remaining_follows = CHASE_MAX;

    while let Some(seg) = remaining_path.next_segment()? {
        // Special handling for going down the tree with RESOLVE_ flags.
        // path_st contains the stat of the root if we're at root, so the
        // length is 1 in that case, and going lower than the root is not
        // allowed here!
        if seg.is_dot_dot {
            if path_st.len() <= 1 && flags.contains(ChaseFlags::RESOLVE_BENEATH) {
                return Err(ChaseError::new(
                    Errno::XDEV,
                    "\"..\" above root with RESOLVE_BENEATH",
                ));
            } else if path_st.len() <= 1 && flags.contains(ChaseFlags::RESOLVE_IN_ROOT) {
                // With RESOLVE_IN_ROOT, we pretend that we hit the real
                // root, and stay there, just like the kernel does.
                continue;
            }
        }

        // Open the next segment. We always use NOFOLLOW here to be able to
        // ensure the RESOLVE flags, and automount behavior.
        let current_fd = current.as_ref().map_or(root, |fd| fd.as_fd());
        let ot_flags = ChaseFlags::NOFOLLOW | (flags & (ChaseFlags::NO_AUTOMOUNT | DEBUG_FLAGS));

        let next_result = open_path(current_fd, &seg.name, ot_flags);

        let next_fd = match callback.as_mut() {
            Some(cb) => cb(next_result, current_fd, &seg.name)?,
            None => next_result?,
        };

        let st = StatxNode::new(next_fd.as_fd(), no_automount)?;

        // We resolve links if: they are not in the last component, or if
        // they are the last component and NOFOLLOW is not set.
        if st.file_type == FileType::Symlink
            && (!seg.is_last || !flags.contains(ChaseFlags::NOFOLLOW))
        {
            // ...however, we do not resolve symlinks with NO_SYMLINKS, and
            // use remaining_follows to ensure we don't loop forever.
            if flags.contains(ChaseFlags::RESOLVE_NO_SYMLINKS) {
                return Err(ChaseError::new(
                    Errno::LOOP,
                    "symlink not allowed with RESOLVE_NO_SYMLINKS",
                ));
            }
            remaining_follows -= 1;
            if remaining_follows <= 0 {
                return Err(ChaseError::new(Errno::LOOP, "too many symlinks"));
            }

            let link = fs::readlinkat(next_fd.as_fd(), "", Vec::new())?.into_bytes();
            let link_is_absolute = link.first() == Some(&b'/');

            if link_is_absolute && flags.contains(ChaseFlags::RESOLVE_BENEATH) {
                return Err(ChaseError::new(
                    Errno::XDEV,
                    "absolute symlink not allowed with RESOLVE_BENEATH",
                ));
            }

            remaining_path.prepend_symlink(&link);

            if link_is_absolute {
                if flags.contains(ChaseFlags::RESOLVE_IN_ROOT) {
                    // If the link was absolute and RESOLVE_IN_ROOT is set,
                    // we resolve the remaining path relative to root_fd.
                    current = None;
                } else {
                    // If the link was absolute, we resolve the remaining
                    // path relative to the real root.
                    current = Some(open_root(flags)?);
                }

                // path_st must only contain the new root at this point.
                let cur = current.as_ref().map_or(root, |fd| fd.as_fd());
                let rst = StatxNode::new(cur, no_automount)?;
                path_st.clear();
                path_st.push(rst);
            }

            continue;
        }

        // Either adds an element to path_st or removes one if we went down
        // the tree. This also checks that going down the tree ends up at the
        // inode we saw before (if we saw it before).
        if seg.is_dot_dot {
            path_st.pop();

            if let Some(lower) = path_st.last() {
                if !st.mount_same(lower) || !st.inode_same(lower) {
                    return Err(ChaseError::new(
                        Errno::XDEV,
                        "parent directory changed during traversal",
                    ));
                }
            }
        } else {
            path_st.push(st);
        }

        // There is still another path component, but the next fd is not a
        // directory. We need the fd to be a directory though, to open the
        // next segment from.
        if !seg.is_last && st.file_type != FileType::Directory {
            return Err(ChaseError::new(
                Errno::NOTDIR,
                "non-final path segment is not a directory",
            ));
        }

        current = Some(next_fd);

        if seg.is_last {
            break;
        }
    }

    match current {
        Some(fd) => Ok(fd),
        // We need an owned fd to return. Not having one can happen if we
        // never finished a single iteration, or if an absolute path with
        // RESOLVE_IN_ROOT makes us point at root_fd. We just re-open to
        // always get an owned fd. This only works because in all these cases
        // the fd is a directory.
        None => Ok(fs::openat(
            root,
            c".",
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?),
    }
}

/// Resolve `path` relative to `dirfd`, returning an `O_PATH` fd.
///
/// If `callback` is provided, it is called for every path component during
/// resolution. For a higher-level interface, see [`Chase`](crate::Chase).
pub fn chaseat(
    dirfd: &impl AsFd,
    path: &CStr,
    flags: ChaseFlags,
    callback: Option<&mut ChaseCallback<'_>>,
) -> Result<OwnedFd, ChaseError> {
    assert!(
        (flags & MUST_BE_FLAGS).bits().count_ones() <= 1,
        "at most one MUST_BE flag"
    );
    assert!(
        !flags.contains(ChaseFlags::MUST_BE_SYMLINK) || flags.contains(ChaseFlags::NOFOLLOW),
        "MUST_BE_SYMLINK requires NOFOLLOW"
    );
    assert!(
        !flags.contains(ChaseFlags::RESOLVE_BENEATH)
            || !flags.contains(ChaseFlags::RESOLVE_IN_ROOT),
        "RESOLVE_BENEATH and RESOLVE_IN_ROOT are mutually exclusive"
    );

    let mut result: Option<OwnedFd> = None;

    // We need the manual impl for NO_AUTOMOUNT, and we can skip this if we
    // don't have openat2 at all.
    if CAN_OPENAT2.load(Ordering::Relaxed)
        && !flags.contains(ChaseFlags::NO_AUTOMOUNT)
        && !flags.contains(ChaseFlags::DEBUG_NO_OPENAT2)
        && callback.is_none()
    {
        let mut oflags = OFlags::PATH | OFlags::CLOEXEC;
        if flags.contains(ChaseFlags::NOFOLLOW) {
            oflags |= OFlags::NOFOLLOW;
        }

        let mut resolve = ResolveFlags::NO_MAGICLINKS;
        if flags.contains(ChaseFlags::RESOLVE_BENEATH) {
            resolve |= ResolveFlags::BENEATH;
        }
        if flags.contains(ChaseFlags::RESOLVE_IN_ROOT) {
            resolve |= ResolveFlags::IN_ROOT;
        }
        if flags.contains(ChaseFlags::RESOLVE_NO_SYMLINKS) {
            resolve |= ResolveFlags::NO_SYMLINKS;
        }

        let r = loop {
            match openat2(dirfd, path, oflags, Mode::empty(), resolve) {
                Err(Errno::AGAIN) => continue,
                other => break other,
            }
        };

        match r {
            Ok(fd) => result = Some(fd),
            // If the syscall is not implemented (ENOSYS) or blocked by
            // seccomp (EPERM), fall back to the manual path chasing.
            Err(Errno::NOSYS | Errno::PERM) => {
                CAN_OPENAT2.store(false, Ordering::Relaxed);
            }
            Err(err) => return Err(err.into()),
        }
    }

    let fd = match result {
        Some(fd) => fd,
        None => chase_manual(dirfd.as_fd(), path, flags, callback)?,
    };

    if flags.intersects(MUST_BE_FLAGS) {
        let no_automount = if flags.contains(ChaseFlags::NO_AUTOMOUNT) {
            AtFlags::NO_AUTOMOUNT
        } else {
            AtFlags::empty()
        };

        let ft = StatxFileType::new(fd.as_fd(), no_automount)?;

        if flags.contains(ChaseFlags::MUST_BE_REGULAR) && ft != FileType::RegularFile {
            return Err(ChaseError::new(
                if ft == FileType::Directory {
                    Errno::ISDIR
                } else {
                    Errno::BADFD
                },
                "not a regular file",
            ));
        }

        if flags.contains(ChaseFlags::MUST_BE_DIRECTORY) && ft != FileType::Directory {
            return Err(ChaseError::new(Errno::NOTDIR, "not a directory"));
        }

        if flags.contains(ChaseFlags::MUST_BE_SOCKET) && ft != FileType::Socket {
            return Err(ChaseError::new(Errno::NOTSOCK, "not a socket"));
        }

        if flags.contains(ChaseFlags::MUST_BE_SYMLINK) && ft != FileType::Symlink {
            return Err(ChaseError::new(Errno::INVAL, "not a symlink"));
        }
    }

    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn chase(dfd: &impl AsFd, path: &CStr, flags: ChaseFlags) -> Result<OwnedFd, ChaseError> {
        chaseat(dfd, path, flags, None)
    }

    fn check_chase(dfd: &impl AsFd, path: &CStr, flags: ChaseFlags, expected_ino: u64) {
        let fd = chase(dfd, path, flags).unwrap();
        assert_eq!(get_ino(fd.as_fd()), expected_ino);

        let fd = chase(dfd, path, flags | ChaseFlags::DEBUG_NO_OPENAT2).unwrap();
        assert_eq!(get_ino(fd.as_fd()), expected_ino);

        let fd = chase(
            dfd,
            path,
            flags | ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::DEBUG_NO_OPEN_TREE,
        )
        .unwrap();
        assert_eq!(get_ino(fd.as_fd()), expected_ino);
    }

    fn check_chase_error(dfd: &impl AsFd, path: &CStr, flags: ChaseFlags, expected: Errno) {
        let err = chase(dfd, path, flags).unwrap_err();
        assert_eq!(err.errno(), expected);

        let err = chase(dfd, path, flags | ChaseFlags::DEBUG_NO_OPENAT2).unwrap_err();
        assert_eq!(err.errno(), expected);

        let err = chase(
            dfd,
            path,
            flags | ChaseFlags::DEBUG_NO_OPENAT2 | ChaseFlags::DEBUG_NO_OPEN_TREE,
        )
        .unwrap_err();
        assert_eq!(err.errno(), expected);
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

    fn open_tmpdir(tmp: &tempfile::TempDir) -> OwnedFd {
        let path = CString::new(tmp.path().to_str().unwrap()).unwrap();
        fs::openat(
            CWD,
            &path,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap()
    }

    fn mkdir_p(tmp: &tempfile::TempDir, rel: &str) -> OwnedFd {
        std::fs::create_dir_all(tmp.path().join(rel)).unwrap();
        let tfd = open_tmpdir(tmp);
        openat_path(tfd.as_fd(), &CString::new(rel).unwrap())
    }

    #[test]
    fn test_chase_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let baz = mkdir_p(&tmp, "file/baz");
        let expected = get_ino(baz.as_fd());

        for path in [
            c"file/baz",
            c"file/baz/",
            c"file/baz/.",
            c"file/baz/../baz",
            c"file////baz/..//baz",
            c"file////baz/..//../file/baz",
        ] {
            check_chase(&tfd, path, ChaseFlags::empty(), expected);
        }

        check_chase_error(&tfd, c"nope", ChaseFlags::empty(), Errno::NOENT);
    }

    #[test]
    fn test_chase_link() {
        let tmp = tempfile::tempdir().unwrap();
        let tfd = open_tmpdir(&tmp);
        let baz = mkdir_p(&tmp, "file/baz");
        let target_ino = get_ino(baz.as_fd());

        symlinkat(c"file/baz", tfd.as_fd(), c"link");
        let link_ino = get_ino(openat_path(tfd.as_fd(), c"link").as_fd());

        check_chase(&tfd, c"link", ChaseFlags::empty(), target_ino);
        check_chase(&tfd, c"link/", ChaseFlags::empty(), target_ino);

        check_chase(&tfd, c"link", ChaseFlags::NOFOLLOW, link_ino);
        check_chase(&tfd, c"link/", ChaseFlags::NOFOLLOW, target_ino);

        check_chase(
            &tfd,
            c"link",
            ChaseFlags::NOFOLLOW | ChaseFlags::RESOLVE_NO_SYMLINKS,
            link_ino,
        );
        check_chase_error(&tfd, c"link", ChaseFlags::RESOLVE_NO_SYMLINKS, Errno::LOOP);
    }

    #[test]
    fn test_chase_resolve() {
        let tmp = tempfile::tempdir().unwrap();

        let foo = mkdir_p(&tmp, "foo");
        let bar = mkdir_p(&tmp, "foo/bar");
        let bar_ino = get_ino(bar.as_fd());

        symlinkat(c"..", foo.as_fd(), c"link1");
        symlinkat(c"bar/../..", foo.as_fd(), c"link2");
        symlinkat(c"/bar", foo.as_fd(), c"link4");

        check_chase(&foo, c"./bar", ChaseFlags::empty(), bar_ino);
        check_chase(&foo, c"../foo/bar", ChaseFlags::empty(), bar_ino);
        check_chase(&foo, c"link1/foo/bar", ChaseFlags::empty(), bar_ino);

        check_chase_error(
            &foo,
            c"link1/foo/bar",
            ChaseFlags::RESOLVE_BENEATH,
            Errno::XDEV,
        );

        check_chase(&foo, c"link1/bar", ChaseFlags::RESOLVE_IN_ROOT, bar_ino);

        check_chase(&foo, c"link4", ChaseFlags::RESOLVE_IN_ROOT, bar_ino);
    }
}
