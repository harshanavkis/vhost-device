#![deny(missing_docs)]
//! vhost-user-vsock device crate

extern crate vhost;
extern crate vhost_user_backend;
extern crate virtio_bindings;
extern crate vm_memory;
extern crate vmm_sys_util;

use core::slice;
use epoll::Events;
use log::*;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::Read;
use std::mem;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::RawFd;
use std::process;
use std::result;
use std::sync::Mutex;
use std::sync::{Arc, RwLock};
use vhost::vhost_user::message::VhostUserProtocolFeatures;
use vhost::vhost_user::message::VhostUserVirtioFeatures;
use vhost::vhost_user::Listener;
use vhost_user_backend::VhostUserBackend;
use vhost_user_backend::VhostUserDaemon;
use vhost_user_backend::Vring;
use vhost_user_backend::VringWorker;
use virtio_bindings::bindings::virtio_blk::__u64;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_NOTIFY_ON_EMPTY;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::eventfd::EFD_NONBLOCK;

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

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
enum Error {
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

struct VhostUserVsockThread {
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    host_sock: RawFd,
    host_listener: UnixListener,
    kill_evt: EventFd,
    vring_worker: Option<Arc<VringWorker>>,
    conn_map: HashMap<RawFd, VsockConnection>,
    epoll_file: File,
}

impl VhostUserVsockThread {
    fn new(uds_path: String) -> Result<Self> {
        // TODO: better error handling
        let host_sock = UnixListener::bind(&uds_path)
            .and_then(|sock| sock.set_nonblocking(true).map(|_| sock))
            .unwrap();

        let epoll_fd = epoll::create(true).map_err(Error::EpollFdCreate)?;
        let epoll_file = unsafe { File::from_raw_fd(epoll_fd) };

        let host_raw_fd = host_sock.as_raw_fd();

        let thread = VhostUserVsockThread {
            mem: None,
            event_idx: false,
            host_sock: host_sock.as_raw_fd(),
            host_listener: host_sock,
            kill_evt: EventFd::new(EFD_NONBLOCK).unwrap(),
            vring_worker: None,
            conn_map: HashMap::new(),
            epoll_file: epoll_file,
        };

        thread.epoll_register(host_raw_fd)?;

        Ok(thread)
    }

    fn epoll_register(&self, fd: RawFd) -> Result<()> {
        epoll::ctl(
            self.epoll_file.as_raw_fd(),
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd,
            epoll::Event::new(epoll::Events::EPOLLIN, fd as u64),
        )
        .map_err(Error::EpollAdd)?;

        Ok(())
    }

    fn get_epoll_fd(&self) -> RawFd {
        self.epoll_file.as_raw_fd()
    }

    fn set_vring_worker(&mut self, vring_worker: Option<Arc<VringWorker>>) {
        self.vring_worker = vring_worker;
        self.vring_worker
            .as_ref()
            .unwrap()
            .register_listener(
                self.get_epoll_fd(),
                epoll::Events::EPOLLIN,
                u64::from(BACKEND_EVENT),
            )
            .unwrap();
    }

    fn register_listener(&mut self, fd: RawFd, ev_type: u16) {
        dbg!("");
        self.vring_worker
            .as_ref()
            .unwrap()
            .register_listener(fd, epoll::Events::EPOLLIN, u64::from(ev_type))
            .unwrap();
    }

    fn process_backend_evt(&mut self, evset: Events) {
        // dbg!("Processing backend event");

        let mut epoll_events = vec![epoll::Event::new(epoll::Events::empty(), 0); 32];
        'epoll: loop {
            match epoll::wait(self.epoll_file.as_raw_fd(), 0, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for i in 0..ev_cnt {
                        self.handle_event(
                            epoll_events[i].data as RawFd,
                            epoll::Events::from_bits(epoll_events[i].events).unwrap(),
                        );
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    warn!("failed to consume new epoll event");
                }
            }
            break 'epoll;
        }
        // Below is a mistake
        // I think we need to bring back nested epoll
        // self.handle_event(self.host_sock, evset)
    }

    fn handle_event(&mut self, fd: RawFd, _evset: epoll::Events) {
        // println!("Fd: {}", fd);
        if fd == self.host_sock {
            dbg!("fd==host_sock");
            self.host_listener
                .accept()
                .map_err(Error::UnixAccept)
                .and_then(|(stream, _)| {
                    stream
                        .set_nonblocking(true)
                        .map(|_| stream)
                        .map_err(Error::UnixAccept)
                })
                .and_then(|stream| self.add_connection(stream))
                .unwrap_or_else(|err| {
                    warn!("Unable to accept new local connection: {:?}", err);
                });
            // let stream = self.conn_map.get_mut(&fd).unwrap();
            // let mut buf = [0 as u8; 20];
            // stream.stream.read(&mut buf[..]).unwrap();
            // println!("{:?}", buf);
        } else {
            // println!("Bitch");
            let stream = self.conn_map.get_mut(&fd).unwrap();
            let mut buf = [0 as u8; 20];
            stream.stream.read(&mut buf[..]).unwrap();
            println!("{:?}", buf);
        }
    }

    fn add_connection(&mut self, stream: UnixStream) -> Result<()> {
        // Create a connection object and add it to the set of connections
        dbg!("Accepting new local connection");
        let stream_fd = stream.as_raw_fd();
        self.conn_map
            .insert(stream_fd, VsockConnection::new(stream));
        self.epoll_register(stream_fd)?;

        // self.register_listener(stream_fd, BACKEND_EVENT);

        Ok(())
    }
}

#[derive(Debug)]
struct VsockConnection {
    stream: UnixStream,
    connect: bool,
}

impl VsockConnection {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream: stream,
            connect: false,
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
        let thread = Mutex::new(VhostUserVsockThread::new(uds_path).unwrap());
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
        // TODO: | 1 << VIRTIO_RING_F_EVENT_IDX
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
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
        let mut vring_rx = vrings[0].write().unwrap();
        let mut vring_tx = vrings[1].write().unwrap();
        let mut work = true;

        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        let mut thread = self.threads[thread_id].lock().unwrap();

        while work {
            work = false;

            match device_event {
                RX_QUEUE_EVENT => {
                    dbg!("RX_QUEUE_EVENT");
                }
                TX_QUEUE_EVENT => {
                    dbg!("TX_QUEUE_EVENT");
                }
                EVT_QUEUE_EVENT => {
                    dbg!("EVT_QUEUE_EVENT");
                }
                BACKEND_EVENT => {
                    // dbg!("BACKEND_EVENT");
                    thread.process_backend_evt(evset);
                }
                _ => {
                    dbg!("Unknown event");
                    return Err(Error::HandleUnknownEvent.into());
                }
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
