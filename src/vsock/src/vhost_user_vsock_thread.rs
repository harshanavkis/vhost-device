#![deny(missing_docs)]

use super::packet::*;
use super::rxops::*;
use super::thread_backend::*;
use super::vsock_conn::*;
use super::ConnMapKey;
use super::Error;
use super::Result;
use super::BACKEND_EVENT;
use super::VSOCK_HOST_CID;
use epoll::Events;
use futures::executor::ThreadPool;
use futures::executor::ThreadPoolBuilder;
use log::warn;
use std::fs::File;
use std::io;
use std::io::Read;
use std::num::Wrapping;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::RawFd;
use std::sync::{Arc, RwLock};
use vhost_user_backend::Vring;
use vhost_user_backend::VringWorker;
use virtio_queue::Queue;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::eventfd::EFD_NONBLOCK;

pub struct VhostUserVsockThread {
    /// Guest memory map
    pub mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    /// VIRTIO_RING_F_EVENT_IDX
    pub event_idx: bool,
    /// Host socket raw file descriptor
    host_sock: RawFd,
    /// Listener listening for new connections on the host
    host_listener: UnixListener,
    /// Used to kill the thread
    pub kill_evt: EventFd,
    /// Instance of VringWorker
    vring_worker: Option<Arc<VringWorker>>,
    /// epoll fd to which new host connections are added
    epoll_file: File,
    /// VsockThreadBackend instance
    pub thread_backend: VsockThreadBackend,
    /// CID of the guest
    guest_cid: u64,
    /// Thread pool to handle event idx
    pool: ThreadPool,
    /// host side port on which application listens
    local_port: u32,
}

impl VhostUserVsockThread {
    /// Create a new instance of VhostUserVsockThread
    pub fn new(uds_path: String, guest_cid: u64) -> Result<Self> {
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
            epoll_file,
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

    /// Register a file with an epoll to listen for events in evset
    pub fn epoll_register(epoll_fd: RawFd, fd: RawFd, evset: epoll::Events) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd,
            epoll::Event::new(evset, fd as u64),
        )
        .map_err(Error::EpollAdd)?;

        Ok(())
    }

    /// Remove a file from the epoll
    pub fn epoll_unregister(epoll_fd: RawFd, fd: RawFd) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_DEL,
            fd,
            epoll::Event::new(epoll::Events::empty(), 0),
        )
        .map_err(Error::EpollRemove)?;

        Ok(())
    }

    /// Modify the events we listen to for the fd in the epoll
    pub fn epoll_modify(epoll_fd: RawFd, fd: RawFd, evset: epoll::Events) -> Result<()> {
        epoll::ctl(
            epoll_fd,
            epoll::ControlOptions::EPOLL_CTL_MOD,
            fd,
            epoll::Event::new(evset, fd as u64),
        )
        .map_err(Error::EpollModify)?;

        Ok(())
    }

    /// Return raw file descriptor of the epoll file
    fn get_epoll_fd(&self) -> RawFd {
        self.epoll_file.as_raw_fd()
    }

    /// Set self's VringWorker
    pub fn set_vring_worker(&mut self, vring_worker: Option<Arc<VringWorker>>) {
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

    /// Process a BACKEND_EVENT received by VhostUserVsockBackend
    pub fn process_backend_evt(&mut self, _evset: Events) {
        let mut epoll_events = vec![epoll::Event::new(epoll::Events::empty(), 0); 32];
        'epoll: loop {
            match epoll::wait(self.epoll_file.as_raw_fd(), 0, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for evt in epoll_events.iter().take(ev_cnt) {
                        self.handle_event(
                            evt.data as RawFd,
                            epoll::Events::from_bits(evt.events).unwrap(),
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

    /// Handle a BACKEND_EVENT by either accepting a new connection or
    /// forwarding a request to the appropriate connection object
    fn handle_event(&mut self, fd: RawFd, evset: epoll::Events) {
        if fd == self.host_sock {
            // This is a new connection initiated by an application running on the host
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
                // New connection from the host
                if evset != epoll::Events::EPOLLIN {
                    // Has to be EPOLLIN as it was not connected previously
                    return;
                }
                let mut unix_stream = self.thread_backend.stream_map.remove(&fd).unwrap();

                // Local peer is sending a "connect PORT\n" command
                let peer_port = Self::read_local_stream_port(&mut unix_stream).unwrap();

                // Allocate a local port number
                let local_port = self.allocate_local_port();

                // Insert the fd into the backend's maps
                self.thread_backend
                    .listener_map
                    .insert(fd, ConnMapKey::new(local_port, peer_port));

                // Create a new connection object an enqueue a connection request
                // packet to be sent to the guest
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

                // Add connection object into the backend's maps
                self.thread_backend
                    .conn_map
                    .insert(conn_map_key, new_vsock_conn);

                self.thread_backend
                    .backend_rxq
                    .push_back(ConnMapKey::new(local_port, peer_port));

                // Re-register the fd to listen for EPOLLIN and EPOLLOUT events
                Self::epoll_modify(
                    self.get_epoll_fd(),
                    fd,
                    epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                )
                .unwrap();
            } else {
                // Previously connected connection
                let key = self.thread_backend.listener_map.get(&fd).unwrap();
                let vsock_conn = self.thread_backend.conn_map.get_mut(&key).unwrap();

                if evset == epoll::Events::EPOLLOUT {
                    // Flush any remaining data from the tx buffer
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
                if connected {
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

    /// Allocate a new local port number
    fn allocate_local_port(&mut self) -> u32 {
        self.local_port += 1;
        self.local_port
    }

    /// Read `CONNECT PORT_NUM\n` from the connected stream
    fn read_local_stream_port(stream: &mut UnixStream) -> Result<u32> {
        let mut buf = [0u8; 32];

        // Minimum number of bytes we should be able to read
        // Corresponds to 'CONNECT 0\n'
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

    /// Add a stream to epoll to listen for EPOLLIN events
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

    /// Iterate over the rx queue and process rx requests
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
                // TODO: Understand why doing the following in the pool works
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

    /// Wrapper to process rx queue based on whether event idx is enabled or not
    pub fn process_rx(&mut self, vring_lock: Arc<RwLock<Vring>>, event_idx: bool) -> Result<bool> {
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

    /// Process tx queue and send requests to the backend for processing
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

    /// Wrapper to process tx queue based on whether event idx is enabled or not
    pub fn process_tx(&mut self, vring_lock: Arc<RwLock<Vring>>, event_idx: bool) -> Result<bool> {
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
