//! The Linux counterpart of `win.rs`: who we are, and who is on the other end
//! of the socket.
//!
//! A unix socket lives in `$XDG_RUNTIME_DIR`, which is 0700 — but a stale socket
//! in `/tmp` (the fallback path) is reachable by anyone who guesses the name.
//! Two defences, both cheap: the socket file is created 0600, and once connected
//! we read SO_PEERCRED and refuse to talk unless the server runs as our own uid.
//! A failure to answer is treated as "not ours": refusing costs one hook event,
//! while trusting it could hand another account every tool call we see.

use std::os::unix::io::AsRawFd;

/// The uid this process runs as.
pub fn uid() -> u32 {
    unsafe { libc_geteuid() }
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// True when the process serving `stream` runs as the same user we do.
pub fn peer_is_same_user(stream: &std::os::unix::net::UnixStream) -> bool {
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    // SOL_SOCKET = 1, SO_PEERCRED = 17 on Linux.
    const SOL_SOCKET: i32 = 1;
    const SO_PEERCRED: i32 = 17;

    let mut cred = Ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe {
        libc_getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut _,
            &mut len,
        )
    };
    if rc != 0 {
        return false;
    }
    cred.uid == uid()
}

extern "C" {
    #[link_name = "getsockopt"]
    fn libc_getsockopt(fd: i32, level: i32, name: i32, value: *mut u32, len: *mut u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn our_own_socket_is_us() {
        let (a, b) = UnixStream::pair().unwrap();
        assert!(peer_is_same_user(&a), "our own process must vouch for itself");
        assert!(peer_is_same_user(&b));
    }

    #[test]
    fn uid_matches_the_os() {
        let out = std::process::Command::new("id").arg("-u").output().unwrap();
        let want: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert_eq!(uid(), want);
    }
}
