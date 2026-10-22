//! The shared memory segment MIT-SHM captures land in (LINUX_SUPPORT.md
//! section 4).
//!
//! A full-screen `GetImage` reply travels through the X socket: for 1920x1080
//! that is 8 MiB per frame, and the round trip is the slow part of a capture.
//! MIT-SHM has the server write the pixels straight into memory this process
//! shares with it, so only the 32-byte reply crosses the connection.
//!
//! The segment is System V shared memory, because that is what the `ShmAttach`
//! request names. It is created private to this process (`IPC_PRIVATE`, mode
//! 0600) and marked for removal the moment it is attached here, so nothing is
//! left behind if the process dies between two captures (LINUX_SUPPORT.md
//! section 4).
//!
//! One segment is created per capture, sized for the largest monitor, and
//! reused for every monitor of that capture: monitors are read one after the
//! other, because one X connection serialises the requests anyway.

use std::ffi::c_void;
use std::ptr;

use x11rb::connection::Connection as _;
use x11rb::protocol::shm;
use x11rb::rust_connection::RustConnection;

/// A System V segment, attached here and in the X server.
///
/// Deliberately not `Clone`, and deliberately kept inside the function that
/// captures: it holds a raw pointer, which `X11Platform` — a `Send + Sync`
/// value — must never contain.
pub(crate) struct Segment<'a> {
    /// The connection the `ShmAttach` went out on, kept so dropping the
    /// segment can detach it again.
    conn: &'a RustConnection,
    /// The X resource id the server knows the segment by.
    seg: shm::Seg,
    /// Where this process sees the segment. Valid until `Drop`.
    addr: *mut c_void,
    /// The segment's size in bytes, as it was created.
    len: usize,
}

impl<'a> Segment<'a> {
    /// Creates a segment of `len` bytes and attaches it to the server.
    ///
    /// # Errors
    ///
    /// The kernel refuses the segment or the mapping, or the server refuses
    /// the attach (a remote `$DISPLAY`, a server without the extension
    /// enabled). The caller logs this and captures through plain `GetImage`
    /// instead, exactly like the Windows GDI fallback (SPEC section 5.4).
    pub(crate) fn new(conn: &'a RustConnection, len: usize) -> Result<Self, String> {
        if len == 0 {
            return Err("a shared memory segment of zero bytes was asked for".to_owned());
        }

        // SAFETY: IPC_PRIVATE asks the kernel for a fresh unnamed segment;
        // the arguments are plain numbers and no pointer is passed.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(format!(
                "shmget({len} bytes) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: shmid is a live segment from shmget, and a null address asks
        // the kernel to choose where it is mapped.
        let addr = unsafe { libc::shmat(shmid, ptr::null(), 0) };
        if addr.is_null() || addr == usize::MAX as *mut c_void {
            let error = std::io::Error::last_os_error();
            // SAFETY: the segment exists and nobody has it attached, so
            // removing it now is the only way to not leak it.
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, ptr::null_mut()) };
            return Err(format!("shmat({shmid}) failed: {error}"));
        }

        // Mark the segment for removal while this process still has it
        // attached: the kernel frees it when the last attachment goes away, so
        // a crash in the middle of a capture cannot leave a segment behind. The
        // server can still attach a segment that is marked for removal.
        //
        // SAFETY: shmid is a live segment id. IPC_RMID neither reads nor writes
        // the third argument.
        if unsafe { libc::shmctl(shmid, libc::IPC_RMID, ptr::null_mut()) } < 0 {
            tracing::warn!(
                shmid,
                error = %std::io::Error::last_os_error(),
                "could not mark the shared memory segment for removal; it may outlive this process"
            );
        }

        let seg = match conn.generate_id() {
            Ok(seg) => seg,
            Err(err) => {
                detach(addr);
                return Err(format!("could not take an X id for the segment: {err}"));
            }
        };
        // The server needs the SysV id, which fits an unsigned 32-bit field.
        let id = u32::try_from(shmid).unwrap_or(0);
        // Not read-only: `ShmGetImage` has the *server* write into the segment.
        let attached = shm::attach(conn, seg, id, false)
            .map_err(|err| err.to_string())
            .and_then(|cookie| cookie.check().map_err(|err| err.to_string()));
        if let Err(err) = attached {
            detach(addr);
            return Err(format!("the X server refused the MIT-SHM segment: {err}"));
        }

        tracing::debug!(bytes = len, shmid, seg, "created the MIT-SHM segment");
        Ok(Self {
            conn,
            seg,
            addr,
            len,
        })
    }

    /// The X resource id `ShmGetImage` names.
    pub(crate) fn seg(&self) -> shm::Seg {
        self.seg
    }

    /// The segment's bytes, as the server wrote them.
    ///
    /// The image data in here is in the root window's pixel format and layout
    /// ([`super::pixels`] turns it into pixels).
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: addr came from a successful shmat and is only detached in
        // Drop, which cannot run while this borrow is alive; len is the size
        // the segment was created with.
        unsafe { std::slice::from_raw_parts(self.addr.cast::<u8>(), self.len) }
    }
}

impl Drop for Segment<'_> {
    fn drop(&mut self) {
        // Detach from the server first, then from this process, so the server
        // never keeps writing into memory that is about to go away.
        match shm::detach(self.conn, self.seg) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => tracing::warn!(error = %err, "could not detach the MIT-SHM segment"),
        }
        let _ = self.conn.flush();
        detach(self.addr);
    }
}

impl std::fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("seg", &self.seg)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// `shmdt`, logged when it fails: a segment that is still attached here cannot
/// be freed by the kernel even though it is marked for removal.
fn detach(addr: *mut c_void) {
    // SAFETY: addr came from a successful shmat and has not been detached yet.
    if unsafe { libc::shmdt(addr.cast_const()) } < 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "could not detach the shared memory segment from this process"
        );
    }
}
