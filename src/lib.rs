#![deny(missing_docs)]
//! vhost-user-vsock device crate

extern crate vhost;
extern crate vhost_user_backend;
extern crate virtio_bindings;
extern crate vm_memory;
extern crate vmm_sys_util;

use byteorder::ByteOrder;
use byteorder::LittleEndian;
use core::slice;
use epoll::Events;
use log::*;
use std::collections::HashMap;
use std::collections::VecDeque;
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
use virtio_queue::DescriptorChain;
use virtio_queue::Queue;
use vm_memory::GuestAddress;
use vm_memory::GuestAddressSpace;
use vm_memory::GuestMemory;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryLoadGuard;
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

// vsock packet header size when packed
const VSOCK_PKT_HDR_SIZE: usize = 44;

// Offset into header for source cid
const HDROFF_SRC_CID: usize = 0;

// Offset into header for destination cid
const HDROFF_DST_CID: usize = 8;

// Offset into header for source port
const HDROFF_SRC_PORT: usize = 16;

// Offset into header for destination port
const HDROFF_DST_PORT: usize = 20;

// Offset into the header for data length
const HDROFF_LEN: usize = 24;

// Offset into header for packet type
const HDROFF_TYPE: usize = 28;

// Offset into header for operation kind
const HDROFF_OP: usize = 30;

// Offset into header for additional flags
// only for VSOCK_OP_SHUTDOWN
const HDROFF_FLAGS: usize = 32;

// Offset into header for tx buf alloc
const HDROFF_BUF_ALLOC: usize = 36;

// Offset into header for forward count
const HDROFF_FWD_CNT: usize = 40;

// CID of the host
const VSOCK_HOST_CID: u64 = 2;

// Connection oriented packet
const VSOCK_TYPE_STREAM: u16 = 1;

// Vsock connection TX buffer capacity
const CONN_TX_BUF_SIZE: u32 = 64 * 1024;

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
    /// No available data
    NoData,
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

#[derive(Clone, Copy)]
enum RxOps {
    /// VSOCK_OP_REQUEST
    Request = 0,
}

impl RxOps {
    /// Convert enum value into bitmask
    fn bitmask(self) -> u8 {
        1u8 << (self as u8)
    }
}

#[derive(Debug)]
struct RxQueue {
    queue: u8,
}

impl RxQueue {
    fn new() -> Self {
        RxQueue { queue: 0 as u8 }
    }
    fn enqueue(&mut self, op: RxOps) {
        self.queue |= op.bitmask();
    }

    fn dequeue(&mut self) -> Option<RxOps> {
        let op = match self.peek() {
            Some(req) => {
                self.queue = self.queue & (!req.clone().bitmask());
                Some(req)
            }
            None => None,
        };

        op
    }

    fn peek(&self) -> Option<RxOps> {
        if self.contains(RxOps::Request as u8) {
            Some(RxOps::Request)
        } else {
            None
        }
    }

    fn contains(&self, op: u8) -> bool {
        self.queue & op != 0
    }

    fn pending_rx(&self) -> bool {
        self.queue != 0
    }
}

/// Vsock packet structure implemented as a wrapper around a virtq descriptor chain:
/// - chain head holds the packet header
/// - optional data descriptor, only present for data packets (VSOCK_OP_RW)
struct VsockPacket {
    hdr: *mut u8,
    buf: Option<*mut u8>,
    buf_size: usize,
}

impl VsockPacket {
    /// Create a vsock packet wrapper around a chain in the rx virtqueue.
    /// Perform bounds checking before creating the wrapper.
    fn from_rx_virtq_head(
        chain: &mut DescriptorChain<GuestMemoryAtomic<GuestMemoryMmap>>,
        mem: GuestMemoryAtomic<GuestMemoryMmap>,
    ) -> Result<Self> {
        // head is at 0, next is at 1, max of two descriptors
        // head contains the packet header
        // next contains the optional packet data
        let mut descr_vec = Vec::with_capacity(2);

        let mut num_descr = 0;
        loop {
            if num_descr >= 2 {
                // Maybe this should be an error?
                break;
            }
            let descr = chain.next().ok_or(Error::QueueMissingDescriptor)?;
            if !descr.is_write_only() {
                return Err(Error::UnwritableDescriptor);
            }
            num_descr += 1;
            descr_vec.push(descr);
        }

        let head_descr = descr_vec[0];
        let data_descr = descr_vec[1];

        if head_descr.len() < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(Error::HdrDescTooSmall(head_descr.len()));
        }

        Ok(Self {
            hdr: VsockPacket::guest_to_host_address(
                &mem.memory(),
                head_descr.addr(),
                VSOCK_PKT_HDR_SIZE,
            )
            .ok_or(Error::GuestMemoryError)? as *mut u8,
            buf: Some(
                VsockPacket::guest_to_host_address(
                    &mem.memory(),
                    data_descr.addr(),
                    data_descr.len() as usize,
                )
                .ok_or(Error::GuestMemoryError)? as *mut u8,
            ),
            buf_size: data_descr.len() as usize,
        })
    }

    /// Convert an absolute address in guest address space to a host
    /// pointer and verify that the provided size defines a valid
    /// range withing a single memory region
    fn guest_to_host_address(
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
        addr: GuestAddress,
        size: usize,
    ) -> Option<*mut u8> {
        if mem.check_range(addr, size) {
            Some(mem.get_host_address(addr).unwrap())
        } else {
            None
        }
    }

    /// In place byte slice access to vsock packet header
    fn hdr(&self) -> &[u8] {
        // Safe as bound checks performed in from_*_virtq_head
        unsafe { std::slice::from_raw_parts(self.hdr as *const u8, VSOCK_PKT_HDR_SIZE) }
    }

    /// In place mutable slice access to vsock packet header
    fn hdr_mut(&mut self) -> &mut [u8] {
        // Safe as bound checks performed in from_*_virtq_head
        unsafe { std::slice::from_raw_parts_mut(self.hdr, VSOCK_PKT_HDR_SIZE) }
    }

    /// Size of vsock packet data, found by accessing len field
    /// of virtio_vsock_hdr struct
    fn len(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_LEN..])
    }

    /// Set the source cid
    fn set_src_cid(&mut self, cid: u64) -> &mut Self {
        LittleEndian::write_u64(&mut self.hdr_mut()[HDROFF_SRC_CID..], cid);
        self
    }

    /// Set the destination cid
    fn set_dst_cid(&mut self, cid: u64) -> &mut Self {
        LittleEndian::write_u64(&mut self.hdr_mut()[HDROFF_DST_CID..], cid);
        self
    }

    /// Set source port
    fn set_src_port(&mut self, port: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_SRC_PORT..], port);
        self
    }

    /// Set destination port
    fn set_dst_port(&mut self, port: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_DST_PORT..], port);
        self
    }

    /// Set type of connection
    fn set_type(&mut self, type_: u16) -> &mut Self {
        LittleEndian::write_u16(&mut self.hdr_mut()[HDROFF_TYPE..], type_);
        self
    }

    /// Set size of tx buf
    fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_BUF_ALLOC..], buf_alloc);
        self
    }

    /// Set amount of tx buf data written to stream
    fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_FWD_CNT..], fwd_cnt);
        self
    }
}

#[derive(Hash, PartialEq, Eq)]
struct ConnMapKey {
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

struct VsockThreadBackend {
    listener_map: HashMap<RawFd, ConnMapKey>,
    conn_map: HashMap<ConnMapKey, VsockConnection>,
    backend_rxq: VecDeque<ConnMapKey>,
}

impl VsockThreadBackend {
    fn new() -> Self {
        Self {
            listener_map: HashMap::new(),
            conn_map: HashMap::new(),
            backend_rxq: VecDeque::new(),
        }
    }

    /// Checks if there are pending rx requests in the backend
    /// rxq
    fn pending_rx(&self) -> bool {
        self.backend_rxq.len() != 0
    }

    /// Deliver a vsock packet to the guest vsock driver
    ///
    /// Returns:
    /// - `Ok(())` if the packet was successfully filled in
    /// - `Err(Error::NoData) if there was no available data
    fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> Result<()> {
        // Pop an event from the backend_rxq
        // TODO: implement recv_pkt for the conn
        let key = self.backend_rxq.pop_front().unwrap();
        let conn = self.conn_map.get_mut(&key).unwrap();

        conn.recv_pkt(pkt);

        Err(Error::NoData)
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
            thread_backend: VsockThreadBackend::new(),
            guest_cid,
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
    }

    fn handle_event(&mut self, fd: RawFd, _evset: epoll::Events) {
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
                .and_then(|stream| self.add_stream_listener(stream))
                .unwrap_or_else(|err| {
                    warn!("Unable to accept new local connection: {:?}", err);
                });
        } else {
            // Check if the stream represented by fd has already establishes a
            // connection with the application running in the guest
            if !self.thread_backend.listener_map.contains_key(&fd) {
                // new connection
                // Local peer is sending a "connect PORT\n" command
                let peer_port =
                    Self::read_local_stream_port(&mut unsafe { UnixStream::from_raw_fd(fd) })
                        .unwrap();
                dbg!("Peer port: {}", peer_port);

                let local_port = Self::allocate_local_port();

                self.thread_backend
                    .listener_map
                    .insert(fd, ConnMapKey::new(local_port, peer_port));

                let conn_map_key = ConnMapKey::new(local_port, peer_port);
                let mut new_vsock_conn = VsockConnection::new(
                    unsafe { UnixStream::from_raw_fd(fd) },
                    VSOCK_HOST_CID,
                    local_port,
                    self.guest_cid,
                );
                new_vsock_conn.rx_queue.enqueue(RxOps::Request);
                new_vsock_conn.set_peer_port(peer_port);

                self.thread_backend
                    .conn_map
                    .insert(conn_map_key, new_vsock_conn);

                self.thread_backend
                    .backend_rxq
                    .push_back(ConnMapKey::new(local_port, peer_port));

                // Unregister stream from the epoll, register when connection is
                // established with the guest
                // Self::epoll_unregister(self.epoll_file.as_raw_fd(), fd).unwrap();
            } else {
                let key = self.thread_backend.listener_map.get(&fd).unwrap();
                let vsock_conn = self.thread_backend.conn_map.get(&key).unwrap();
                let connected = vsock_conn.connect;
                if connected.clone() {
                    // TODO: If connected add a request to the queue
                } else {
                }
            }
        }
    }

    fn allocate_local_port() -> u32 {
        // TODO: A better way of doing this, should work as a quick hack
        0
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
        // Create a connection object and add it to the set of connections
        dbg!("Accepting new local connection");
        let stream_fd = stream.as_raw_fd();
        // self.thread_backend.listener_map.insert(stream_fd, false);
        self.epoll_register(stream_fd)?;

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

        let a = atomic_mem.memory();

        let mut used_desc_heads = [(0, 0); QUEUE_SIZE as usize];
        let mut used_count = 0 as usize;

        while let Some(mut avail_desc) = queue.iter().map_err(|_| Error::IterateQueue)?.next() {
            used_any = true;
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
        }
        Ok(false)
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
                queue.disable_notification().unwrap();
                self.process_rx_queue(queue, vring_lock.clone())?;
                if !queue.enable_notification().unwrap() {
                    break;
                }
            }
        } else {
        }
        Ok(false)
    }

    fn process_tx(&mut self, vring_lock: Arc<RwLock<Vring>>, event_idx: bool) -> Result<bool> {
        Ok(false)
    }
}

#[derive(Debug)]
struct VsockConnection {
    stream: UnixStream,
    connect: bool,
    peer_port: u32,
    rx_queue: RxQueue,
    local_cid: u64,
    local_port: u32,
    guest_cid: u64,
    /// Total number of bytes written to stream from tx buffer
    fwd_cnt: Wrapping<u32>,
}

impl VsockConnection {
    fn new(stream: UnixStream, local_cid: u64, local_port: u32, guest_cid: u64) -> Self {
        Self {
            stream: stream,
            connect: false,
            peer_port: 0,
            rx_queue: RxQueue::new(),
            local_cid,
            local_port,
            guest_cid,
            fwd_cnt: Wrapping(0),
        }
    }

    fn set_peer_port(&mut self, peer_port: u32) {
        self.peer_port = peer_port;
    }

    fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> Result<()> {
        self.init_pkt(pkt);
        Ok(())
    }

    fn init_pkt<'a>(&self, pkt: &'a mut VsockPacket) -> &'a mut VsockPacket {
        // Zero out the packet header
        for b in pkt.hdr_mut() {
            *b = 0;
        }

        pkt.set_src_cid(self.local_cid)
            .set_dst_cid(self.guest_cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(VSOCK_TYPE_STREAM)
            .set_buf_alloc(CONN_TX_BUF_SIZE)
            .set_fwd_cnt(self.fwd_cnt.0)
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
        let vring_rx_lock = vrings[0].clone();
        let vring_tx_lock = vrings[1].clone();

        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        let mut thread = self.threads[thread_id].lock().unwrap();
        let evt_idx = thread.event_idx;

        match device_event {
            RX_QUEUE_EVENT => {
                dbg!("RX_QUEUE_EVENT");
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_rx_lock.clone(), evt_idx)?;
                }
            }
            TX_QUEUE_EVENT => {
                dbg!("TX_QUEUE_EVENT");
                thread.process_tx(vring_tx_lock.clone(), evt_idx)?;
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_tx_lock.clone(), evt_idx)?;
                }
            }
            EVT_QUEUE_EVENT => {
                dbg!("EVT_QUEUE_EVENT");
            }
            BACKEND_EVENT => {
                dbg!("BACKEND_EVENT");
                thread.process_backend_evt(evset);
                thread.process_tx(vring_tx_lock.clone(), evt_idx)?;
                if thread.thread_backend.pending_rx() {
                    thread.process_rx(vring_rx_lock.clone(), evt_idx)?;
                }
            }
            _ => {
                dbg!("Unknown event");
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
