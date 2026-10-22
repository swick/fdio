// SPDX-License-Identifier: MIT OR Apache-2.0
// SPDX-FileCopyrightText: Copyright © the fdio contributors

use rustix::io::Errno;
use std::any::{Any, TypeId};
use std::ffi::CStr;
use std::os::unix::io::{AsFd, BorrowedFd, OwnedFd};

use crate::engine::{chaseat, ChaseCallback, ChaseError, ChaseFlags};

/// A step in the chase callback chain.
///
/// Each step receives the result of opening the current path component (or
/// the output of the previous step) and can transform it. Steps are called
/// in order for every component during manual path resolution.
pub trait ChaseStep {
    type Output: 'static;

    fn step(
        &mut self,
        result: Result<OwnedFd, Errno>,
        parent: BorrowedFd<'_>,
        segment: &CStr,
    ) -> Result<OwnedFd, Errno>;

    /// Called after the chase completes. Return accumulated results.
    fn finish(&mut self) -> Option<Self::Output> {
        None
    }
}

pub(crate) trait ErasedStep {
    fn step(
        &mut self,
        result: Result<OwnedFd, Errno>,
        parent: BorrowedFd<'_>,
        segment: &CStr,
    ) -> Result<OwnedFd, Errno>;

    fn finish_erased(&mut self) -> Option<(TypeId, Box<dyn Any>)>;
}

impl<T: ChaseStep + 'static> ErasedStep for T {
    fn step(
        &mut self,
        result: Result<OwnedFd, Errno>,
        parent: BorrowedFd<'_>,
        segment: &CStr,
    ) -> Result<OwnedFd, Errno> {
        ChaseStep::step(self, result, parent, segment)
    }

    fn finish_erased(&mut self) -> Option<(TypeId, Box<dyn Any>)> {
        ChaseStep::finish(self).map(|v| (TypeId::of::<T>(), Box::new(v) as Box<dyn Any>))
    }
}

pub struct ChaseResult {
    fd: OwnedFd,
    results: Vec<(TypeId, Box<dyn Any>)>,
}

impl ChaseResult {
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    pub fn get<S: ChaseStep + 'static>(&self) -> Option<&S::Output> {
        self.results
            .iter()
            .find(|(id, _)| *id == TypeId::of::<S>())
            .and_then(|(_, v)| v.downcast_ref::<S::Output>())
    }
}

impl AsFd for ChaseResult {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Composable path resolution returning an `O_PATH` fd.
///
/// For the capability-based API that returns [`FileRef`](crate::FileRef),
/// use the resolve methods on `FileRef<Root>` or `FileRef<Dir>` instead.
pub struct Chase<'a> {
    dirfd: BorrowedFd<'a>,
    flags: ChaseFlags,
    steps: Vec<&'a mut dyn ErasedStep>,
}

impl<'a> Chase<'a> {
    pub fn new(dirfd: &'a impl AsFd, flags: ChaseFlags) -> Self {
        Self {
            dirfd: dirfd.as_fd(),
            flags,
            steps: Vec::new(),
        }
    }

    /// Add a custom [`ChaseStep`] to the chain.
    pub fn step<S: ChaseStep + 'static>(mut self, step: &'a mut S) -> Self {
        self.steps.push(step);
        self
    }

    /// Execute the chase, resolving `path`.
    ///
    /// After resolution, `finish()` is called on each step and results
    /// are collected into [`ChaseResult`].
    pub fn resolve(self, path: &CStr) -> Result<ChaseResult, ChaseError> {
        let mut steps = self.steps;

        let fd = if steps.is_empty() {
            chaseat(&self.dirfd, path, self.flags, None)?
        } else {
            let mut cb: &mut ChaseCallback<'_> = &mut |result, parent, segment| {
                let mut r = result;
                for step in steps.iter_mut() {
                    r = step.step(r, parent, segment);
                }
                r
            };
            chaseat(&self.dirfd, path, self.flags, Some(&mut cb))?
        };

        let results = steps.iter_mut().filter_map(|s| s.finish_erased()).collect();

        Ok(ChaseResult { fd, results })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{self, Mode, OFlags, CWD};
    use std::ffi::CString;

    struct ComponentCounter(u32);

    impl ChaseStep for ComponentCounter {
        type Output = u32;

        fn step(
            &mut self,
            result: Result<OwnedFd, Errno>,
            _parent: BorrowedFd<'_>,
            _segment: &CStr,
        ) -> Result<OwnedFd, Errno> {
            self.0 += 1;
            result
        }

        fn finish(&mut self) -> Option<u32> {
            Some(self.0)
        }
    }

    #[test]
    fn test_chase_step_finish_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();

        let root = CString::new(tmp.path().to_str().unwrap()).unwrap();
        let dirfd = fs::openat(
            CWD,
            &root,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .unwrap();

        let mut counter = ComponentCounter(0);
        let result = Chase::new(&dirfd, ChaseFlags::empty())
            .step(&mut counter)
            .resolve(c"a/b/c")
            .unwrap();

        assert_eq!(result.get::<ComponentCounter>(), Some(&3));
    }
}
