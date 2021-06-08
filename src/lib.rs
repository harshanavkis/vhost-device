#![deny(missing_docs)]
//! vhost-user-vsock device crate

#[derive(Debug)]
/// This structure is the public API through which an external program
/// is allowed to configure the backend
pub struct VsockConfig {
    guest_cid: u32,
    socket: String,
    uds_path: String,
}

impl VsockConfig {
    /// Create a new instance of the VsockConfig struct, containing the
    /// parameters to be fed into the vsock-backend server
    pub fn new(guest_cid: u32, socket: String, uds_path: String) -> Self {
        VsockConfig {
            guest_cid: guest_cid,
            socket: socket,
            uds_path: uds_path,
        }
    }

    /// Return the guest's current CID
    pub fn get_guest_cid(&self) -> &u32 {
        &self.guest_cid
    }

    /// Return the path of the unix domain socket which is listening to
    /// requests from the host side application
    pub fn get_uds_path(&self) -> &str {
        &self.uds_path
    }

    /// Return the path of the unix domain socket which is listening to
    /// requests from the guest.
    pub fn get_socket_path(&self) -> &str {
        &self.socket
    }
}

/// This is the public API through which an external program starts the
/// vhost-user-vsock backend server.
fn start_backend_server(vsock_config: VsockConfig) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vsock_config_setup() {
        let vsock_config = VsockConfig::new(
            3,
            "/tmp/vhost4.socket".to_string(),
            "/tmp/vm4.vsock".to_string(),
        );

        assert_eq!(*vsock_config.get_guest_cid(), 3);
        assert_eq!(vsock_config.get_socket_path(), "/tmp/vhost4.socket");
        assert_eq!(vsock_config.get_uds_path(), "/tmp/vm4.vsock");
    }
}
