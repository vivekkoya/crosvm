// Copyright 2021 The Chromium OS Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Structs for Unix Domain Socket listener and connection.

use std::any::Any;
use std::fs::File;
use std::io::ErrorKind;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::path::Path;
use std::path::PathBuf;

use base::AsRawDescriptor;
use base::RawDescriptor;
use base::SafeDescriptor;
use base::ScmSocket;

use crate::connection::Listener;
use crate::message::*;
use crate::unix::SystemListener;
use crate::Connection;
use crate::Error;
use crate::Result;
use crate::SystemStream;

/// Unix domain socket listener for accepting incoming connections.
pub struct SocketListener {
    fd: SystemListener,
    drop_path: Option<Box<dyn Any>>,
}

impl SocketListener {
    /// Create a unix domain socket listener.
    ///
    /// # Return:
    /// * - the new SocketListener object on success.
    /// * - SocketError: failed to create listener socket.
    pub fn new<P: AsRef<Path>>(path: P, unlink: bool) -> Result<Self> {
        if unlink {
            let _ = std::fs::remove_file(&path);
        }
        let fd = SystemListener::bind(&path).map_err(Error::SocketError)?;

        struct DropPath {
            path: PathBuf,
        }

        impl Drop for DropPath {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.path);
            }
        }

        Ok(SocketListener {
            fd,
            drop_path: Some(Box::new(DropPath {
                path: path.as_ref().to_owned(),
            })),
        })
    }

    /// Take and return the resources that the parent process needs to keep alive as long as the
    /// child process lives, in case of incoming fork.
    pub fn take_resources_for_parent(&mut self) -> Option<Box<dyn Any>> {
        self.drop_path.take()
    }
}

impl Listener for SocketListener {
    /// Accept an incoming connection.
    ///
    /// # Return:
    /// * - Some(SystemListener): new SystemListener object if new incoming connection is available.
    /// * - None: no incoming connection available.
    /// * - SocketError: errors from accept().
    fn accept(&mut self) -> Result<Option<Connection<MasterReq>>> {
        loop {
            match self.fd.accept() {
                Ok((stream, _addr)) => {
                    return Ok(Some(Connection::from(stream)));
                }
                Err(e) => {
                    match e.kind() {
                        // No incoming connection available.
                        ErrorKind::WouldBlock => return Ok(None),
                        // New connection closed by peer.
                        ErrorKind::ConnectionAborted => return Ok(None),
                        // Interrupted by signals, retry
                        ErrorKind::Interrupted => continue,
                        _ => return Err(Error::SocketError(e)),
                    }
                }
            }
        }
    }

    /// Change blocking status on the listener.
    ///
    /// # Return:
    /// * - () on success.
    /// * - SocketError: failure from set_nonblocking().
    fn set_nonblocking(&self, block: bool) -> Result<()> {
        self.fd.set_nonblocking(block).map_err(Error::SocketError)
    }
}

impl AsRawDescriptor for SocketListener {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.fd.as_raw_descriptor()
    }
}

/// Unix domain socket based vhost-user connection.
pub struct SocketPlatformConnection {
    sock: ScmSocket<SystemStream>,
}

// TODO: Switch to TryFrom to avoid the unwrap.
impl From<SystemStream> for SocketPlatformConnection {
    fn from(sock: SystemStream) -> Self {
        Self {
            sock: sock.try_into().unwrap(),
        }
    }
}

// Advance the internal cursor of the slices.
// This is same with a nightly API `IoSlice::advance_slices` but for `&[u8]`.
fn advance_slices(bufs: &mut &mut [&[u8]], mut count: usize) {
    use std::mem::take;

    let mut idx = 0;
    for b in bufs.iter() {
        if count < b.len() {
            break;
        }
        count -= b.len();
        idx += 1;
    }
    *bufs = &mut take(bufs)[idx..];
    if !bufs.is_empty() {
        bufs[0] = &bufs[0][count..];
    }
}

impl SocketPlatformConnection {
    /// Create a new stream by connecting to server at `str`.
    ///
    /// # Return:
    /// * - the new SocketPlatformConnection object on success.
    /// * - SocketConnect: failed to connect to peer.
    pub fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let sock = SystemStream::connect(path).map_err(Error::SocketConnect)?;
        Ok(Self::from(sock))
    }

    /// Sends all bytes from scatter-gather vectors with optional attached file descriptors. Will
    /// loop until all data has been transfered.
    ///
    /// # TODO
    /// This function takes a slice of `&[u8]` instead of `IoSlice` because the internal
    /// cursor needs to be moved by `advance_slices()`.
    /// Once `IoSlice::advance_slices()` becomes stable, this should be updated.
    /// <https://github.com/rust-lang/rust/issues/62726>.
    fn send_iovec_all(
        &self,
        mut iovs: &mut [&[u8]],
        mut fds: Option<&[RawDescriptor]>,
    ) -> Result<()> {
        // Guarantee that `iovs` becomes empty if it doesn't contain any data.
        advance_slices(&mut iovs, 0);

        while !iovs.is_empty() {
            let iovec: Vec<_> = iovs.iter_mut().map(|i| IoSlice::new(i)).collect();
            match self.sock.send_vectored_with_fds(&iovec, fds.unwrap_or(&[])) {
                Ok(n) => {
                    fds = None;
                    advance_slices(&mut iovs, n);
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock | ErrorKind::Interrupted => {}
                    _ => return Err(Error::SocketError(e)),
                },
            }
        }
        Ok(())
    }

    /// Sends a single message over the socket with optional attached file descriptors.
    ///
    /// - `hdr`: vhost message header
    /// - `body`: vhost message body (may be empty to send a header-only message)
    /// - `payload`: additional bytes to append to `body` (may be empty)
    pub fn send_message(
        &self,
        hdr: &[u8],
        body: &[u8],
        payload: &[u8],
        fds: Option<&[RawDescriptor]>,
    ) -> Result<()> {
        let mut iobufs = [hdr, body, payload];
        self.send_iovec_all(&mut iobufs, fds)
    }

    /// Reads bytes from the socket into the given scatter/gather vectors with optional attached
    /// file.
    ///
    /// The underlying communication channel is a Unix domain socket in STREAM mode. It's a little
    /// tricky to pass file descriptors through such a communication channel. Let's assume that a
    /// sender sending a message with some file descriptors attached. To successfully receive those
    /// attached file descriptors, the receiver must obey following rules:
    ///   1) file descriptors are attached to a message.
    ///   2) message(packet) boundaries must be respected on the receive side.
    /// In other words, recvmsg() operations must not cross the packet boundary, otherwise the
    /// attached file descriptors will get lost.
    /// Note that this function wraps received file descriptors as `File`.
    ///
    /// # Return:
    /// * - (number of bytes received, [received files]) on success
    /// * - Disconnect: the connection is closed.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn recv_into_bufs(
        &self,
        bufs: &mut [IoSliceMut],
        allow_fd: bool,
    ) -> Result<(usize, Option<Vec<File>>)> {
        let max_fds = if allow_fd { MAX_ATTACHED_FD_ENTRIES } else { 0 };
        let (bytes, fds) = self.sock.recv_vectored_with_fds(bufs, max_fds)?;

        // 0-bytes indicates that the connection is closed.
        if bytes == 0 {
            return Err(Error::Disconnect);
        }

        let files = if fds.is_empty() {
            None
        } else {
            Some(fds.into_iter().map(File::from).collect())
        };

        Ok((bytes, files))
    }
}

impl AsRawDescriptor for SocketPlatformConnection {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.sock.as_raw_descriptor()
    }
}

impl AsMut<SystemStream> for SocketPlatformConnection {
    fn as_mut(&mut self) -> &mut SystemStream {
        self.sock.inner_mut()
    }
}

/// Convert a `SafeDescriptor` to a `UnixStream`.
///
/// # Safety
///
/// `file` must represent a unix domain socket.
pub unsafe fn to_system_stream(fd: SafeDescriptor) -> Result<SystemStream> {
    Ok(fd.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::unix::tests::temp_dir;

    #[test]
    fn create_listener() {
        let dir = temp_dir();
        let mut path = dir.path().to_owned();
        path.push("sock");
        let listener = SocketListener::new(&path, true).unwrap();

        assert!(listener.as_raw_descriptor() > 0);
    }

    #[test]
    fn accept_connection() {
        let dir = temp_dir();
        let mut path = dir.path().to_owned();
        path.push("sock");
        let mut listener = SocketListener::new(&path, true).unwrap();
        listener.set_nonblocking(true).unwrap();

        // accept on a fd without incoming connection
        let conn = listener.accept().unwrap();
        assert!(conn.is_none());
    }

    #[test]
    fn test_advance_slices() {
        // Test case from https://doc.rust-lang.org/std/io/struct.IoSlice.html#method.advance_slices
        let buf1 = [1; 8];
        let buf2 = [2; 16];
        let buf3 = [3; 8];
        let mut bufs = &mut [&buf1[..], &buf2[..], &buf3[..]][..];
        advance_slices(&mut bufs, 10);
        assert_eq!(bufs[0], [2; 14].as_ref());
        assert_eq!(bufs[1], [3; 8].as_ref());
    }
}
