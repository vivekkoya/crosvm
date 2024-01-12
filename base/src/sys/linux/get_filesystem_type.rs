// Copyright 2021 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fs::File;
use std::mem::MaybeUninit;
use std::os::unix::io::AsRawFd;

use libc::fstatfs64;

use super::Result;
use crate::syscall;

/// Obtain file system type of the file system that the file is served from.
#[allow(clippy::unnecessary_cast)]
pub fn get_filesystem_type(file: &File) -> Result<i64> {
    let mut statfs_buf = MaybeUninit::<libc::statfs64>::uninit();
    // SAFETY:
    // Safe because we just got the memory space with exact required amount and
    // passing that on.
    syscall!(unsafe { fstatfs64(file.as_raw_fd(), statfs_buf.as_mut_ptr()) })?;
    // SAFETY:
    // Safe because the kernel guarantees the struct is initialized.
    let statfs_buf = unsafe { statfs_buf.assume_init() };
    Ok(statfs_buf.f_type as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simple_test() {
        let file = File::open("/dev/null").unwrap();
        let _fstype = get_filesystem_type(&file).unwrap();
    }
}
