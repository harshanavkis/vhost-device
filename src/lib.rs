#![deny(missing_docs)]
//! vhost-user-vsock device crate

extern crate vhost;
extern crate vhost_user_backend;
extern crate virtio_bindings;
extern crate vm_memory;

use core::slice;
use std::io;
use std::mem;
use std::process;
use std::result;
use std::sync::{Arc, RwLock};
use vhost::vhost_user::message::VhostUserProtocolFeatures;
use vhost::vhost_user::message::VhostUserVirtioFeatures;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserBackend;
use vhost_user_backend::VhostUserDaemon;
use vhost_user_backend::Vring;
use virtio_bindings::bindings::virtio_blk::__u64;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_VERSION_1;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryMmap;

#[derive(Debug)]
/// This structure is the public API through which an external program
/// is allowed to configure the backend
pub struct VsockConfig {
    guest_cid: u64,
    socket: String,
    uds_path: String,
}

impl VsockConfig {
    /// Create a new instance of the VsockConfig struct, containing the
    /// parameters to be fed into the vsock-backend server
    pub fn new(guest_cid: u64, socket: String, uds_path: String) -> Self {
        VsockConfig {
            guest_cid: guest_cid,
            socket: socket,
            uds_path: uds_path,
        }
    }

    /// Return the guest's current CID
    pub fn get_guest_cid(&self) -> &u64 {
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

struct VhostUserVsockBackend {
    event_idx: bool,
    guest_cid: __u64,
}

impl VhostUserVsockBackend {
    fn new(guest_cid: u64) -> Self {
        VhostUserVsockBackend {
            event_idx: false,
            guest_cid: guest_cid,
        }
    }
}

impl VhostUserBackend for VhostUserVsockBackend {
    fn num_queues(&self) -> usize {
        2
    }

    fn max_queue_size(&self) -> usize {
        128
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1 | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(
        &mut self,
        _atomic_mem: GuestMemoryAtomic<GuestMemoryMmap>,
    ) -> result::Result<(), io::Error> {
        Ok(())
    }

    fn handle_event(
        &self,
        _device_event: u16,
        _evset: epoll::Events,
        _vrings: &[Arc<RwLock<Vring>>],
        _thread_id: usize,
    ) -> result::Result<bool, io::Error> {
        Ok(true)
    }

    fn get_config(&self, _offset: u32, _size: u32) -> Vec<u8> {
        let buf = unsafe {
            slice::from_raw_parts(
                &self.guest_cid as *const __u64 as *const _,
                mem::size_of::<__u64>(),
            )
        };
        buf.to_vec()
    }
}

/// This is the public API through which an external program starts the
/// vhost-user-vsock backend server.
pub fn start_backend_server(vsock_config: VsockConfig) {
    let vsock_backend = Arc::new(RwLock::new(VhostUserVsockBackend::new(
        vsock_config.guest_cid,
    )));

    let listener = Listener::new(vsock_config.socket, true).unwrap();

    let name = "vhost-user-vsock";
    let mut vsock_daemon = VhostUserDaemon::new(name.to_string(), vsock_backend.clone()).unwrap();
    if let Err(e) = vsock_daemon.start(listener) {
        dbg!("Failed to start vsock daemon: {:?}", e);
        process::exit(1);
    }

    if let Err(e) = vsock_daemon.wait() {
        dbg!("Error from daemon main thread: {:?}", e);
        process::exit(1);
    }
}

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
