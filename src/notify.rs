//! systemd readiness notification (`sd_notify` protocol).

/// Sends a state message (e.g. `READY=1`) to systemd through `$NOTIFY_SOCKET`.
/// It is a no-op when not started by systemd with `Type=notify`, and any
/// failure is ignored on purpose: notification is best-effort.
pub fn sd_notify(message: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let addr = match bytes.strip_prefix(b"@") {
        // A leading '@' denotes a Linux abstract socket.
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(&path),
    };
    if let Ok(addr) = addr {
        let _ = sock.send_to_addr(message.as_bytes(), &addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn message_reaches_the_notify_socket() {
        let path = std::env::temp_dir().join(format!("iowatchd-notify-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let server = UnixDatagram::bind(&path).unwrap();
        std::env::set_var("NOTIFY_SOCKET", &path);
        sd_notify("READY=1");
        std::env::remove_var("NOTIFY_SOCKET");
        let mut buf = [0u8; 64];
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let n = server.recv(&mut buf).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn without_a_socket_it_is_a_noop() {
        sd_notify("READY=1");
    }
}
