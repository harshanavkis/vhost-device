#![deny(missing_docs)]
//! vhost-user-vsock device crate

extern crate vhost;
extern crate vhost_user_backend;
extern crate virtio_bindings;
extern crate vm_memory;
extern crate vmm_sys_util;

use core::slice;
use epoll::Events;
use futures::executor::ThreadPool;
use futures::executor::ThreadPoolBuilder;
use log::*;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::Read;
use std::mem;
use std::num::Wrapping;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::RawFd;
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
use vhost_user_backend::VringWorker;
use virtio_bindings::bindings::virtio_blk::__u64;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_NOTIFY_ON_EMPTY;
use virtio_bindings::bindings::virtio_net::VIRTIO_F_VERSION_1;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::Queue;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::eventfd::EFD_NONBLOCK;

mod packet;
mod txbuf;
use packet::*;
mod rxops;
use rxops::*;
mod rxqueue;
mod vsock_conn;
use vsock_conn::*;
mod thread_backend;
use thread_backend::*;

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

struct VhostUserVsockThread {
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    host_sock: RawFd,
    host_listener: UnixListener,
    kill_evt: EventFd,
    vring_worker: Option<Arc<VringWorker>>,
    epoll_file: File,
    thread_backend: VsockThreadBackend,
    guest_cid: u64,
    pool: ThreadPool,
    local_port: u32,
}

impl VhostUserVsockThread {
    fn new(uds_path: String, guest_cid: u64) -> Result<Self> {
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
            epoll_file: epoll_file,
            thread_backend: VsockThreadBackend::new(uds_path, epoll_fd),
            guest_cid,
            pool: ThreadPoolBuilder::new()
                .pool_size(1)
                .create()
                .map_err(Error::CreateThreadPool)?,
            local_port: 0,
        };

        VhostUserVsockThread::epoll_register(epoll_fd, host_raw_fd, epoll::Events::EPOLLIN)?;

        Ok(thread)
    }

    fn epoll_register(epoll_fd: RawFd, fd: RawFd, evset: epoll::Events) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd,
            epoll::Event::new(evset, fd as u64),
        )
        .map_err(Error::EpollAdd)?;

        Ok(())
    }

    fn epoll_unregister(epoll_fd: RawFd, fd: RawFd) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            fd,
            epoll::Event::new(epoll::Events::empty(), 0),
        )
        .map_err(Error::EpollRemove)?;

        Ok(())
    }

    fn epoll_modify(epoll_fd: RawFd, fd: RawFd, evset: epoll::Events) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_MOD,
            fd,
            epoll::Event::new(evset, fd as u64),
        )
        .map_err(Error::EpollModify)?;

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

    fn process_backend_evt(&mut self, _evset: Events) {
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
    }

    fn handle_event(&mut self, fd: RawFd, evset: epoll::Events) {
        if fd == self.host_sock {
            self.host_listener
                .accept()
                .map_err(Error::UnixAccept)
                .and_then(|(stream, _)| {
                    stream
                        .set_nonblocking(true)
                        .map(|_| stream)
                        .map_err(Error::UnixAccept)
                })
                .and_then(|stream| self.add_stream_listener(stream))
                .unwrap_or_else(|err| {
                    println!("Unable to accept new local conn: {:?}", err);
                    warn!("Unable to accept new local connection: {:?}", err);
                });
        } else {
            // Check if the stream represented by fd has already established a
            // connection with the application running in the guest
            if !self.thread_backend.listener_map.contains_key(&fd) {
                if evset != epoll::Events::EPOLLIN {
                    return;
                }
                let mut unix_stream = self.thread_backend.stream_map.remove(&fd).unwrap();
                // new connection
                // Local peer is sending a "connect PORT\n" command
                let peer_port = Self::read_local_stream_port(&mut unix_stream).unwrap();

                let local_port = self.allocate_local_port();

                self.thread_backend
                    .listener_map
                    .insert(fd, ConnMapKey::new(local_port, peer_port));

                let conn_map_key = ConnMapKey::new(local_port, peer_port);
                let mut new_vsock_conn = VsockConnection::new_local_init(
                    unix_stream,
                    VSOCK_HOST_CID,
                    local_port,
                    self.guest_cid,
                    peer_port,
                    self.get_epoll_fd(),
                );
                new_vsock_conn.rx_queue.enqueue(RxOps::Request);
                new_vsock_conn.set_peer_port(peer_port);

                self.thread_backend
                    .conn_map
                    .insert(conn_map_key, new_vsock_conn);

                self.thread_backend
                    .backend_rxq
                    .push_back(ConnMapKey::new(local_port, peer_port));

                Self::epoll_modify(
                    self.get_epoll_fd(),
                    fd,
                    epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                )
                .unwrap();
            } else {
                let key = self.thread_backend.listener_map.get(&fd).unwrap();
                let vsock_conn = self.thread_backend.conn_map.get_mut(&key).unwrap();

                if evset == epoll::Events::EPOLLOUT {
                    match vsock_conn.tx_buf.flush_to(&mut vsock_conn.stream) {
                        Ok(cnt) => {
                            vsock_conn.fwd_cnt += Wrapping(cnt as u32);
                            vsock_conn.rx_queue.enqueue(RxOps::CreditUpdate);
                            self.thread_backend.backend_rxq.push_back(ConnMapKey::new(
                                vsock_conn.local_port,
                                vsock_conn.peer_port,
                            ));
                        }
                        Err(e) => {
                            dbg!("Error: {:?}", e);
                        }
                    }
                    return;
                }

                // TODO: This probably always evaluates to true in this block
                let connected = vsock_conn.connect;
                if connected.clone() {
                    // Unregister stream from the epoll, register when connection is
                    // established with the guest
                    Self::epoll_unregister(self.epoll_file.as_raw_fd(), fd).unwrap();

                    // Enqueue a read request
                    vsock_conn.rx_queue.enqueue(RxOps::Rw);
                    self.thread_backend
                        .backend_rxq
                        .push_back(ConnMapKey::new(vsock_conn.local_port, vsock_conn.peer_port));
                } else {
                }
            }
        }
    }

    fn allocate_local_port(&mut self) -> u32 {
        self.local_port += 1;
        self.local_port
    }

    fn read_local_stream_port(stream: &mut UnixStream) -> Result<u32> {
        let mut buf = [0u8; 32];

        // Minimum number of bytes we should be able to read
        const MIN_READ_LEN: usize = 10;

        // Read in the minimum number of bytes we can read
        stream
            .read_exact(&mut buf[..MIN_READ_LEN])
            .map_err(Error::UnixRead)?;

        let mut read_len = MIN_READ_LEN;
        while buf[read_len - 1] != b'\n' && read_len < buf.len() {
            stream
                .read_exact(&mut buf[read_len..read_len + 1])
                .map_err(Error::UnixRead)?;
            read_len += 1;
        }

        let mut word_iter = std::str::from_utf8(&buf[..read_len])
            .map_err(Error::ConvertFromUtf8)?
            .split_whitespace();

        word_iter
            .next()
            .ok_or(Error::InvalidPortRequest)
            .and_then(|word| {
                if word.to_lowercase() == "connect" {
                    Ok(())
                } else {
                    Err(Error::InvalidPortRequest)
                }
            })
            .and_then(|_| word_iter.next().ok_or(Error::InvalidPortRequest))
            .and_then(|word| word.parse::<u32>().map_err(Error::ParseInteger))
            .map_err(|e| Error::ReadStreamPort(Box::new(e)))
    }

    fn add_stream_listener(&mut self, stream: UnixStream) -> Result<()> {
        let stream_fd = stream.as_raw_fd();
        self.thread_backend.stream_map.insert(stream_fd, stream);
        VhostUserVsockThread::epoll_register(
            self.get_epoll_fd(),
            stream_fd,
            epoll::Events::EPOLLIN,
        )?;

        // self.register_listener(stream_fd, BACKEND_EVENT);
        Ok(())
    }

    fn process_rx_queue(
        &mut self,
        queue: &mut Queue<GuestMemoryAtomic<GuestMemoryMmap>>,
        vring_lock: Arc<RwLock<Vring>>,
    ) -> Result<bool> {
        let mut used_any = false;
        let atomic_mem = match &self.mem {
            Some(m) => m,
            None => return Err(Error::NoMemoryConfigured),
        };

        while let Some(mut avail_desc) = queue.iter().map_err(|_| Error::IterateQueue)?.next() {
            used_any = true;
            let atomic_mem = atomic_mem.clone();

            let head_idx = avail_desc.head_index();
            let used_len =
                match VsockPacket::from_rx_virtq_head(&mut avail_desc, atomic_mem.clone()) {
                    Ok(mut pkt) => {
                        if self.thread_backend.recv_pkt(&mut pkt).is_ok() {
                            pkt.hdr().len() + pkt.len() as usize
                        } else {
                            queue.go_to_previous_position();
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("vsock: RX queue error: {:?}", e);
                        0
                    }
                };

            let vring_lock = vring_lock.clone();
            let event_idx = self.event_idx;

            self.pool.spawn_ok(async move {
                let mut vring = vring_lock.write().unwrap();
                if event_idx {
                    let queue = vring.mut_queue();
                    if queue.add_used(head_idx, used_len as u32).is_err() {
                        warn!("Could not return used descriptors to ring");
                    }
                    match queue.needs_notification() {
                        Err(_) => {
                            warn!("Could not check if queue needs to be notified");
                            vring.signal_used_queue().unwrap();
                        }
                        Ok(needs_notification) => {
                            if needs_notification {
                                vring.signal_used_queue().unwrap();
                            }
                        }
                    }
                } else {
                    if vring
                        .mut_queue()
                        .add_used(head_idx, used_len as u32)
                        .is_err()
                    {
                        warn!("Could not return used descriptors to ring");
                    }
                    vring.signal_used_queue().unwrap();
                }
            });

            if !self.thread_backend.pending_rx() {
                break;
            }
        }
        Ok(used_any)
    }

    fn process_rx(&mut self, vring_lock: Arc<RwLock<Vring>>, event_idx: bool) -> Result<bool> {
        let mut vring = vring_lock.write().unwrap();
        let queue = vring.mut_queue();
        if event_idx {
            // To properly handle EVENT_IDX we need to keep calling
            // process_rx_queue until it stops finding new requests
            // on the queue, as vm-virtio's Queue implementation
            // only checks avail_index once
            loop {
                if !self.thread_backend.pending_rx() {
                    break;
                }
                queue.disable_notification().unwrap();
                // TODO: This should not even occur as pending_rx is checked before
                // calling this function, figure out why
                self.process_rx_queue(queue, vring_lock.clone())?;
                if !queue.enable_notification().unwrap() {
                    break;
                }
                // TODO: This may not be required because of
                // previous pending_rx check
                // if !work {
                //     break;
                // }
            }
        } else {
            self.process_rx_queue(queue, vring_lock.clone())?;
        }
        Ok(false)
    }

    fn process_tx_queue(
        &mut self,
        queue: &mut Queue<GuestMemoryAtomic<GuestMemoryMmap>>,
        vring_lock: Arc<RwLock<Vring>>,
    ) -> Result<bool> {
        let mut used_any = false;

        let atomic_mem = match &self.mem {
            Some(m) => m,
            None => return Err(Error::NoMemoryConfigured),
        };

        while let Some(mut avail_desc) = queue.iter().map_err(|_| Error::IterateQueue)?.next() {
            used_any = true;
            let atomic_mem = atomic_mem.clone();

            let head_idx = avail_desc.head_index();
            let pkt = match VsockPacket::from_tx_virtq_head(&mut avail_desc, atomic_mem.clone()) {
                Ok(pkt) => pkt,
                Err(e) => {
                    dbg!("vsock: error reading TX packet: {:?}", e);
                    continue;
                }
            };

            if self.thread_backend.send_pkt(&pkt).is_err() {
                queue.go_to_previous_position();
                break;
            }

            // TODO: Check if the protocol requires read length to be correct
            let used_len = 0;

            let vring_lock = vring_lock.clone();
            let event_idx = self.event_idx;

            self.pool.spawn_ok(async move {
                let mut vring = vring_lock.write().unwrap();
                if event_idx {
                    let queue = vring.mut_queue();
                    if queue.add_used(head_idx, used_len as u32).is_err() {
                        warn!("Could not return used descriptors to ring");
                    }
                    match queue.needs_notification() {
                        Err(_) => {
                            warn!("Could not check if queue needs to be notified");
                            vring.signal_used_queue().unwrap();
                        }
                        Ok(needs_notification) => {
                            if needs_notification {
                                vring.signal_used_queue().unwrap();
                            }
                        }
                    }
                } else {
                    if vring
                        .mut_queue()
                        .add_used(head_idx, used_len as u32)
                        .is_err()
                    {
                        warn!("Could not return used descriptors to ring");
                    }
                    vring.signal_used_queue().unwrap();
                }
            });
        }

        Ok(used_any)
    }

    fn process_tx(&mut self, vring_lock: Arc<RwLock<Vring>>, event_idx: bool) -> Result<bool> {
        let mut vring = vring_lock.write().unwrap();
        let queue = vring.mut_queue();

        if event_idx {
            // To properly handle EVENT_IDX we need to keep calling
            // process_rx_queue until it stops finding new requests
            // on the queue, as vm-virtio's Queue implementation
            // only checks avail_index once
            loop {
                queue.disable_notification().unwrap();
                self.process_tx_queue(queue, vring_lock.clone())?;
                if !queue.enable_notification().unwrap() {
                    break;
                }
            }
        } else {
            self.process_tx_queue(queue, vring_lock.clone())?;
        }
        Ok(false)
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
