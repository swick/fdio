// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: Copyright © the fdio contributors

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, StatxFlags};
use rustix::io::Errno;
use std::ffi::{CStr, CString};
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};

use crate::chase::ChaseStep;
use crate::engine::ChaseFlags;

pub struct ChaseStepMkdir {
    mode: Mode,
}

impl ChaseStepMkdir {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }
}

impl ChaseStep for ChaseStepMkdir {
    type Output = ();

    fn step(
        &mut self,
        result: Result<OwnedFd, Errno>,
        parent: BorrowedFd<'_>,
        segment: &CStr,
    ) -> Result<OwnedFd, Errno> {
        match result {
            Ok(fd) => Ok(fd),
            Err(Errno::NOENT) => {
                if let Err(e) = fs::mkdirat(parent, segment, self.mode) {
                    if e != Errno::EXIST {
                        return Err(e);
                    }
                }
                Ok(fs::openat(
                    parent,
                    segment,
                    OFlags::CLOEXEC | OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?)
            }
            Err(e) => Err(e),
        }
    }
}

pub struct ChaseStepPath {
    components: Vec<Vec<u8>>,
    last_symlink_name: Option<Vec<u8>>,
    flags: ChaseFlags,
}

impl ChaseStepPath {
    pub fn new(flags: ChaseFlags) -> Self {
        Self {
            components: Vec::new(),
            last_symlink_name: None,
            flags,
        }
    }
}

impl ChaseStep for ChaseStepPath {
    type Output = CString;

    fn step(
        &mut self,
        result: Result<OwnedFd, Errno>,
        _parent: BorrowedFd<'_>,
        segment: &CStr,
    ) -> Result<OwnedFd, Errno> {
        let fd = result?;
        let name = segment.to_bytes();

        if name == b".." {
            if self.components.last().map_or(true, |c| c == b"..") {
                self.components.push(b"..".to_vec());
            } else {
                self.components.pop();
            }
            self.last_symlink_name = None;
            return Ok(fd);
        }

        let no_automount = if self.flags.contains(ChaseFlags::NO_AUTOMOUNT) {
            AtFlags::NO_AUTOMOUNT
        } else {
            AtFlags::empty()
        };
        let st = fs::statx(
            fd.as_fd(),
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW | no_automount,
            StatxFlags::TYPE,
        )?;
        let ft = FileType::from_raw_mode(u32::from(st.stx_mode));

        if ft == FileType::Symlink {
            let target = fs::readlinkat(fd.as_fd(), "", Vec::new())?.into_bytes();
            if target.first() == Some(&b'/') {
                self.components.clear();
            }
            self.last_symlink_name = Some(name.to_vec());
        } else {
            self.components.push(name.to_vec());
            self.last_symlink_name = None;
        }

        Ok(fd)
    }

    fn finish(&mut self) -> Option<CString> {
        if self.flags.contains(ChaseFlags::NOFOLLOW) {
            if let Some(name) = self.last_symlink_name.take() {
                self.components.push(name);
            }
        }
        if self.components.is_empty() {
            Some(CString::new(".").unwrap())
        } else {
            let mut buf = Vec::new();
            for (i, c) in self.components.iter().enumerate() {
                if i > 0 {
                    buf.push(b'/');
                }
                buf.extend_from_slice(c);
            }
            Some(CString::new(buf).unwrap())
        }
    }
}
