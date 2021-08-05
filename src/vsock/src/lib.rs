#![deny(missing_docs)]
//! vhost-user-vsock device crate

extern crate vhost;
extern crate vhost_user_backend;
extern crate virtio_bindings;
extern crate vm_memory;
extern crate vmm_sys_util;

use core::slice;
use log::*;
use std::fmt;
use std::io;
use std::mem;
use std::process;
use std::result;
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use std::u16;
use std::u32;
use std::u64;
use std::u8;
use vhost::vhost_user::message::VhostUserProtocolFeatures;
use vhost::vhost_user::message::VhostUserVirtioFeatures;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserBackend;
use vhost_user_backend::VhostUserDaemon;
use vhost_user_backend::Vring;
use virtio_bindings::bindings::virtio_blk::__u64;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_NOTIFY_ON_EMPTY;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;

use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryMmap;

mod packet;
mod rxops;
mod txbuf;
use rxops::*;
mod rxqueue;
mod thread_backend;
mod vhost_user_vsock_thread;
mod vsock_conn;
use vhost_user_vsock_thread::*;

const NUM_QUEUES: usize = 2;
const QUEUE_SIZE: usize = 256;

// New descriptors pending on the rx queue
const RX_QUEUE_EVENT: u16 = 0;
// New descriptors are pending on the tx queue.
const TX_QUEUE_EVENT: u16 = 1;
// New descriptors are pending on the event queue.
const EVT_QUEUE_EVENT: u16 = 2;
// Notification coming from the backend.
const BACKEND_EVENT: u16 = 3;

// Vsock connection TX buffer capacity
// TODO: Make this value configurable
const CONN_TX_BUF_SIZE: u32 = 64 * 1024;

// CID of the host
const VSOCK_HOST_CID: u64 = 2;

// Connection oriented packet
const VSOCK_TYPE_STREAM: u16 = 1;

// Vsock packet operation ID
//
// Connection request
const VSOCK_OP_REQUEST: u16 = 1;
// Connection response
const VSOCK_OP_RESPONSE: u16 = 2;
// Connection reset
const VSOCK_OP_RST: u16 = 3;
// Shutdown connection
const VSOCK_OP_SHUTDOWN: u16 = 4;
// Data read/write
const VSOCK_OP_RW: u16 = 5;
// Flow control credit update
const VSOCK_OP_CREDIT_UPDATE: u16 = 6;
// Flow control credit request
const VSOCK_OP_CREDIT_REQUEST: u16 = 7;

// Vsock packet flags
//
// VSOCK_OP_SHUTDOWN: Packet sender will receive no more data
const VSOCK_FLAGS_SHUTDOWN_RCV: u32 = 1;
// VSOCK_OP_SHUTDOWN: Packet sender will send no more data
const VSOCK_FLAGS_SHUTDOWN_SEND: u32 = 2;

type Result<T> = std::result::Result<T, Error>;

/// Below enum defines custom error types
#[derive(Debug)]
pub enum Error {
    /// Failed to handle event other than EPOLLIN event
    HandleEventNotEpollIn,
    /// Failed to handle unknown event
    HandleUnknownEvent,
    /// Failed to accept new local socket connection
    UnixAccept(std::io::Error),
    /// Failed to create an epoll fd
    EpollFdCreate(std::io::Error),
    /// Failed to add to epoll
    EpollAdd(std::io::Error),
    /// Failed to modify evset associated with epoll
    EpollModify(std::io::Error),
    /// Failed to read from unix stream
    UnixRead(std::io::Error),
    /// Failed to convert byte array to string
    ConvertFromUtf8(std::str::Utf8Error),
    /// Invalid vsock connection request from host
    InvalidPortRequest,
    /// Unable to convert string to integer
    ParseInteger(std::num::ParseIntError),
    /// Error reading stream port
    ReadStreamPort(Box<Error>),
    /// Failed to de-register fd from epoll
    EpollRemove(std::io::Error),
    /// No memory configured
    NoMemoryConfigured,
    /// Unable to iterate queue
    IterateQueue,
    /// Missing descriptor in queue
    QueueMissingDescriptor,
    /// Unable to write to the descriptor
    UnwritableDescriptor,
    /// Small header descriptor
    HdrDescTooSmall(u32),
    /// Chained guest memory error
    GuestMemoryError,
    /// No rx request available
    NoRequestRx,
    /// Unable to create thread pool
    CreateThreadPool(std::io::Error),
    /// Unable to read from descriptor
    UnreadableDescriptor,
    /// Data buffer size less than size in packet header
    DataDescTooSmall,
    /// Packet missing data buffer
    PktBufMissing,
    /// Failed to connect to unix socket
    UnixConnect(std::io::Error),
    /// Unable to write to unix stream
    UnixWrite,
    /// Unable to push data to local tx buffer
    LocalTxBufFull,
    /// Unable to flush data from local tx buffer
    LocalTxBufFlush(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vhost_user_vsock_error: {:?}", self)
    }
}

impl std::error::Error for Error {}

impl std::convert::From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::new(io::ErrorKind::Other, e)
    }
}

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

/// A local port and peer port pair used to retrieve
/// the corresponding connection
#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct ConnMapKey {
    local_port: u32,
    peer_port: u32,
}

impl ConnMapKey {
    fn new(local_port: u32, peer_port: u32) -> Self {
        Self {
            local_port,
            peer_port,
        }
    }
}

struct VhostUserVsockBackend {
    guest_cid: __u64,
    threads: Vec<Mutex<VhostUserVsockThread>>,
    queues_per_thread: Vec<u64>,
}

impl VhostUserVsockBackend {
    fn new(guest_cid: u64, uds_path: String) -> Result<Self> {
        let mut threads = Vec::new();
        let thread = Mutex::new(VhostUserVsockThread::new(uds_path, guest_cid).unwrap());
        let mut queues_per_thread = Vec::new();
        queues_per_thread.push(0b11);
        threads.push(thread);

        Ok(Self {
            guest_cid: guest_cid,
            threads: threads,
            queues_per_thread: queues_per_thread,
        })
    }
}

impl VhostUserBackend for VhostUserVsockBackend {
    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
            | 1 << VIRTIO_RING_F_EVENT_IDX
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::CONFIG
    }

    fn set_event_idx(&mut self, enabled: bool) {
        for thread in self.threads.iter() {
            thread.lock().unwrap().event_idx = enabled;
        }
    }

    fn update_memory(
        &mut self,
        atomic_mem: GuestMemoryAtomic<GuestMemoryMmap>,
    ) -> result::Result<(), io::Error> {
        for thread in self.threads.iter() {
            thread.lock().unwrap().mem = Some(atomic_mem.clone());
        }
        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        evset: epoll::Events,
        vrings: &[Arc<RwLock<Vring>>],
        thread_id: usize,
    ) -> result::Result<bool, io::Error> {
        let vring_rx_lock = vrings[0].clone();
        let vring_tx_lock = vrings[1].clone();

        if evset == epoll::Events::EPOLLOUT {
            dbg!("received epollout");
        }

        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        let mut thread = self.threads[thread_id].lock().unwrap();
        let evt_idx = thread.event_idx;

        match device_event {
            RX_QUEUE_EVENT => {
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_rx_lock.clone(), evt_idx)?;
                }
            }
            TX_QUEUE_EVENT => {
                thread.process_tx(vring_tx_lock.clone(), evt_idx)?;
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_rx_lock.clone(), evt_idx)?;
                }
            }
            EVT_QUEUE_EVENT => {}
            BACKEND_EVENT => {
                thread.process_backend_evt(evset);
                thread.process_tx(vring_tx_lock.clone(), evt_idx)?;
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_rx_lock.clone(), evt_idx)?;
                }
            }
            _ => {
                return Err(Error::HandleUnknownEvent.into());
            }
        }

        Ok(false)
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

    fn queues_per_thread(&self) -> Vec<u64> {
        self.queues_per_thread.clone()
    }
}

/// This is the public API through which an external program starts the
/// vhost-user-vsock backend server.
pub fn start_backend_server(vsock_config: VsockConfig) {
    let vsock_backend = Arc::new(RwLock::new(
        VhostUserVsockBackend::new(vsock_config.guest_cid, vsock_config.uds_path).unwrap(),
    ));

    let listener = Listener::new(vsock_config.socket, true).unwrap();

    let name = "vhost-user-vsock";
    let mut vsock_daemon = VhostUserDaemon::new(name.to_string(), vsock_backend.clone()).unwrap();

    let mut vring_workers = vsock_daemon.get_vring_workers();

    if vring_workers.len() != vsock_backend.read().unwrap().threads.len() {
        error!("Number of vring workers must be identical to number of backend threads");
    }

    for thread in vsock_backend.read().unwrap().threads.iter() {
        thread
            .lock()
            .unwrap()
            .set_vring_worker(Some(vring_workers.remove(0)));
    }
    if let Err(e) = vsock_daemon.start(listener) {
        dbg!("Failed to start vsock daemon: {:?}", e);
        process::exit(1);
    }

    if let Err(e) = vsock_daemon.wait() {
        dbg!("Error from daemon main thread: {:?}", e);
        process::exit(1);
    }

    debug!("Vsock daemon is finished");

    for thread in vsock_backend.read().unwrap().threads.iter() {
        if let Err(e) = thread.lock().unwrap().kill_evt.write(1) {
            error!("Error shutting down worker thread: {:?}", e);
        }
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
