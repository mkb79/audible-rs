//! Windows named-pipe transport for the ephemeral plugin broker (AUD-280).
//!
//! On Unix the broker's access control is the `0700` directory holding its
//! socket; the Windows analogue is a named pipe whose security descriptor
//! restricts it to the current user's SID. That parity is what lets
//! [`crate::session::rpc::serve_pipe`] stamp accepted connections
//! `trusted = true` as honestly as `serve_unix` does over the unix socket —
//! preserving the `hosts`-scope behaviour a plugin gets on Unix. The pipe
//! is byte-mode and rejects remote clients (tokio defaults); the descriptor
//! adds owner-only access on top. Windows-only.

use std::ffi::{OsString, c_void};
use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

/// An owner-only security descriptor for a named pipe: a protected DACL
/// granting the current user's SID full control and nobody else — the
/// analogue of the Unix `0700` socket dir. Owns the descriptor allocation,
/// freed on drop.
struct OwnerOnlySecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

// The descriptor is a plain heap allocation, never shared or mutated after
// construction; the listener that owns it lives on a single task.
unsafe impl Send for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    /// Builds the descriptor from the current process token's user SID.
    fn current_user() -> io::Result<Self> {
        let sid = current_user_sid_string()?;
        // `D:P(A;;GA;;;<sid>)` — a protected DACL (`P`: no inherited ACEs)
        // whose single ACE grants GENERIC_ALL (`GA`) to the user; everyone
        // else is denied by omission.
        let sddl = format!("D:P(A;;GA;;;{sid})");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string; on success the
        // call stores a LocalAlloc'd descriptor that `Drop` frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    /// A `SECURITY_ATTRIBUTES` pointing at this descriptor. The value
    /// borrows `self` for the duration of one pipe-creation call.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: freeing the descriptor allocated in `current_user`.
            unsafe { LocalFree(self.descriptor as HLOCAL) };
        }
    }
}

/// Reads the current process's user SID as an `S-1-…` string.
fn current_user_sid_string() -> io::Result<String> {
    // SAFETY: a documented Win32 sequence; the token handle and the string
    // SID allocation are both released before returning, and every call's
    // success is checked before its output is used.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            // First call sizes the buffer (it "fails", writing the needed
            // length to `len`); the second fills it.
            let mut len: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buf = vec![0u8; len as usize];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token_user = &*buf.as_ptr().cast::<TOKEN_USER>();
            let mut sid_str: PWSTR = std::ptr::null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str) == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid = pwstr_to_string(sid_str);
            LocalFree(sid_str as HLOCAL);
            Ok(sid)
        })();
        CloseHandle(token);
        result
    }
}

/// Copies a NUL-terminated wide string into a `String`.
///
/// # Safety
/// `p` must point at a valid NUL-terminated UTF-16 string.
unsafe fn pwstr_to_string(p: PWSTR) -> String {
    let mut len = 0usize;
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    String::from_utf16_lossy(slice)
}

/// A named-pipe "listener": a pipe has no bound socket object, so this
/// holds only the name + security needed to mint fresh server instances on
/// demand — which is how the accept loop works on Windows.
pub struct PipeListener {
    name: OsString,
    security: OwnerOnlySecurity,
}

impl PipeListener {
    /// Prepares a listener for `name` (`\\.\pipe\…`), restricted to the
    /// current user's SID.
    pub fn new(name: impl Into<OsString>) -> io::Result<Self> {
        Ok(Self {
            name: name.into(),
            security: OwnerOnlySecurity::current_user()?,
        })
    }

    /// Creates a server instance. Pass `first = true` for the very first
    /// one — it sets `FILE_FLAG_FIRST_PIPE_INSTANCE` so no other process can
    /// squat the name — and `false` for every re-arm. The broker creates the
    /// first instance **synchronously before spawning the accept loop**, so
    /// the pipe exists in the namespace the moment `Broker::start` returns
    /// (the analogue of `UnixListener::bind`); otherwise a client that
    /// connects before the accept task runs would get `os error 2`.
    pub fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = self.security.attributes();
        let mut options = ServerOptions::new();
        options.first_pipe_instance(first);
        // SAFETY: `attributes` and the descriptor it points at (owned by
        // `self.security`) outlive this synchronous call.
        let server = unsafe {
            options.create_with_security_attributes_raw(
                &self.name,
                (&raw mut attributes).cast::<c_void>(),
            )
        }?;
        Ok(server)
    }
}
