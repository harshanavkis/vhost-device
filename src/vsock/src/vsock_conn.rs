#![deny(missing_docs)]

use log::info;

use super::packet::*;
use super::rxops::*;
use super::rxqueue::*;
use super::txbuf::*;
use super::Error;
use super::Result;
use super::VhostUserVsockThread;
use super::{
    CONN_TX_BUF_SIZE, VSOCK_FLAGS_SHUTDOWN_RCV, VSOCK_FLAGS_SHUTDOWN_SEND, VSOCK_OP_CREDIT_REQUEST,
    VSOCK_OP_CREDIT_UPDATE, VSOCK_OP_REQUEST, VSOCK_OP_RESPONSE, VSOCK_OP_RST, VSOCK_OP_RW,
    VSOCK_OP_SHUTDOWN, VSOCK_TYPE_STREAM,
};
use std::io::Read;
use std::io::Write;
use std::os::unix::prelude::AsRawFd;
use std::{
    io::ErrorKind,
    num::Wrapping,
    os::unix::{net::UnixStream, prelude::RawFd},
};

#[derive(Debug)]
pub struct VsockConnection {
    /// Host-side stream corresponding to this vsock connection
    pub stream: UnixStream,
    /// Specifies if the stream is connected to a listener on the host
    pub connect: bool,
    /// Port at which a guest application is listening to
    pub peer_port: u32,
    /// Queue holding pending rx operations per connection
    pub rx_queue: RxQueue,
    /// CID of the host
    local_cid: u64,
    /// Port on the host at which a host-side application listens to
    pub local_port: u32,
    /// CID of the guest
    pub guest_cid: u64,
    /// Total number of bytes written to stream from tx buffer
    pub fwd_cnt: Wrapping<u32>,
    /// Total number of bytes previously forwarded to stream
    last_fwd_cnt: Wrapping<u32>,
    /// Size of buffer the guest has allocated for this connection
    peer_buf_alloc: u32,
    /// Number of bytes the peer has forwarded to a connection
    peer_fwd_cnt: Wrapping<u32>,
    /// The total number of bytes sent to the guest vsock driver
    rx_cnt: Wrapping<u32>,
    /// epoll fd to which this connection's stream has to be added
    pub epoll_fd: RawFd,
    /// Local tx buffer
    pub tx_buf: LocalTxBuf,
}

impl VsockConnection {
    /// Create a new vsock connection object for locally i.e host-side
    /// inititated connections.
    pub fn new_local_init(
        stream: UnixStream,
        local_cid: u64,
        local_port: u32,
        guest_cid: u64,
        guest_port: u32,
        epoll_fd: RawFd,
    ) -> Self {
        Self {
            stream,
            connect: false,
            peer_port: guest_port,
            rx_queue: RxQueue::new(),
            local_cid,
            local_port,
            guest_cid,
            fwd_cnt: Wrapping(0),
            last_fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            epoll_fd,
            tx_buf: LocalTxBuf::new(),
        }
    }

    /// Create a new vsock connection object for connections initiated by
    /// an application running in the guest.
    pub fn new_peer_init(
        stream: UnixStream,
        local_cid: u64,
        local_port: u32,
        guest_cid: u64,
        guest_port: u32,
        epoll_fd: RawFd,
        peer_buf_alloc: u32,
    ) -> Self {
        // TODO: Create a separate new for guest initiated connections
        let mut rx_queue = RxQueue::new();
        rx_queue.enqueue(RxOps::Response);
        Self {
            stream,
            connect: false,
            peer_port: guest_port,
            rx_queue,
            local_cid,
            local_port,
            guest_cid,
            fwd_cnt: Wrapping(0),
            last_fwd_cnt: Wrapping(0),
            peer_buf_alloc,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            epoll_fd,
            tx_buf: LocalTxBuf::new(),
        }
    }

    /// Set the peer port to the guest side application's port
    pub fn set_peer_port(&mut self, peer_port: u32) {
        self.peer_port = peer_port;
    }

    /// Process a vsock packet that is meant for this connection.
    /// Forward data to the host-side application if the vsock packet
    /// contains a RW operation.
    pub fn recv_pkt(&mut self, pkt: &mut VsockPacket) -> Result<()> {
        // Initialize all fields in the packet header
        self.init_pkt(pkt);

        match self.rx_queue.dequeue() {
            Some(RxOps::Request) => {
                // Send a connection request to the guest-side application
                pkt.set_op(VSOCK_OP_REQUEST);
                Ok(())
            }
            Some(RxOps::Rw) => {
                if !self.connect {
                    // There is no host-side application listening for this
                    // packet, hence send back an RST.
                    pkt.set_op(VSOCK_OP_RST);
                    return Ok(());
                }

                // Check if peer has space for receiving data
                if self.need_credit_update_from_peer() {
                    // TODO: Fix this, TX_EVENT not got after sending this packet
                    self.last_fwd_cnt = self.fwd_cnt;
                    pkt.set_op(VSOCK_OP_CREDIT_REQUEST);
                    return Ok(());
                }
                let buf = pkt.buf_mut().ok_or(Error::PktBufMissing)?;

                // Perform a credit check to find the maximum read size. The read
                // data must fit inside a packet buffer and be within peer's
                // available buffer space
                let max_read_len = std::cmp::min(buf.len(), self.peer_avail_credit());

                // Read data from the stream directly into the buffer
                if let Ok(read_cnt) = self.stream.read(&mut buf[..max_read_len]) {
                    if read_cnt == 0 {
                        // If no data was read then the stream was closed down unexpectedly.
                        // Send a shutdown packet to the guest-side application.
                        pkt.set_op(VSOCK_OP_SHUTDOWN)
                            .set_flag(VSOCK_FLAGS_SHUTDOWN_RCV)
                            .set_flag(VSOCK_FLAGS_SHUTDOWN_SEND);
                    } else {
                        // If data was read, then set the length field in the packet header
                        // to the amount of data that was read.
                        pkt.set_op(VSOCK_OP_RW).set_len(read_cnt as u32);

                        // Re-register the stream file descriptor for read and write events
                        VhostUserVsockThread::epoll_register(
                            self.epoll_fd,
                            self.stream.as_raw_fd(),
                            epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                        )?;
                    }

                    // Update the rx_cnt with the amount of data in the vsock packet.
                    self.rx_cnt += Wrapping(pkt.len());
                    self.last_fwd_cnt = self.fwd_cnt;
                }
                Ok(())
            }
            Some(RxOps::Response) => {
                // A response has been received to a newly initiated host-side connection
                self.connect = true;
                pkt.set_op(VSOCK_OP_RESPONSE);
                Ok(())
            }
            Some(RxOps::CreditUpdate) => {
                // Request credit update from the guest.
                if !self.rx_queue.pending_rx() {
                    // Waste an rx buffer if no rx is pending
                    pkt.set_op(VSOCK_OP_CREDIT_UPDATE);
                    self.last_fwd_cnt = self.fwd_cnt;
                }
                Ok(())
            }
            _ => Err(Error::NoRequestRx),
        }
    }

    /// Deliver a guest generated packet to this connection
    ///
    /// Returns:
    /// - always `Ok(())` to indicate that the packet has been consumed
    pub fn send_pkt(&mut self, pkt: &VsockPacket) -> Result<()> {
        // Update peer credit information
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        if pkt.op() == VSOCK_OP_RESPONSE {
            // Confirmation for a host initiated connection
            // TODO: Handle stream write error in a better manner
            let response = format!("OK {}\n", self.peer_port);
            self.stream.write_all(response.as_bytes()).unwrap();
            self.connect = true;
        } else if pkt.op() == VSOCK_OP_RW {
            // Data has to be written to the host-side stream
            if pkt.buf().is_none() {
                info!(
                    "Dropping empty packet from guest (lp={}, pp={})",
                    self.local_port, self.peer_port
                );
                return Ok(());
            }

            let buf_slice = &pkt.buf().unwrap()[..(pkt.len() as usize)];
            if let Err(err) = self.send_bytes(buf_slice) {
                // TODO: Terminate this connection
                dbg!("err:{:?}", err);
                return Ok(());
            }
        } else if pkt.op() == VSOCK_OP_CREDIT_UPDATE {
            // Already updated the credit

            // Re-register the stream file descriptor for read and write events
            if VhostUserVsockThread::epoll_modify(
                self.epoll_fd,
                self.stream.as_raw_fd(),
                epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
            )
            .is_err()
            {
                VhostUserVsockThread::epoll_register(
                    self.epoll_fd,
                    self.stream.as_raw_fd(),
                    epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                )
                .unwrap();
            };
        } else if pkt.op() == VSOCK_OP_CREDIT_REQUEST {
            // Send back this connection's credit information
            self.rx_queue.enqueue(RxOps::CreditUpdate);
        } else if pkt.op() == VSOCK_OP_SHUTDOWN {
            // Shutdown this connection
            let recv_off = pkt.flags() & VSOCK_FLAGS_SHUTDOWN_RCV != 0;
            let send_off = pkt.flags() & VSOCK_FLAGS_SHUTDOWN_SEND != 0;

            if recv_off && send_off && self.tx_buf.is_empty() {
                self.rx_queue.enqueue(RxOps::Rst);
            }
        }

        Ok(())
    }

    /// Write data to the host-side stream
    ///
    /// Returns:
    /// - Ok(cnt) where cnt is the number of bytes written to the stream
    /// - Err(Error::UnixWrite) if there was an error writing to the stream
    fn send_bytes(&mut self, buf: &[u8]) -> Result<()> {
        if !self.tx_buf.is_empty() {
            // Data is already present in the buffer and the backend
            // is waiting for a EPOLLOUT event to flush it
            return self.tx_buf.push(buf);
        }

        // Write data to the stream
        let written_count = match self.stream.write(buf) {
            Ok(cnt) => cnt,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    0
                } else {
                    println!("send_bytes error: {:?}", e);
                    return Err(Error::UnixWrite);
                }
            }
        };

        // Increment forwarded count by number of bytes written to the stream
        self.fwd_cnt += Wrapping(written_count as u32);
        self.rx_queue.enqueue(RxOps::CreditUpdate);

        if written_count != buf.len() {
            return self.tx_buf.push(&buf[written_count..]);
        }

        Ok(())
    }

    /// Initialize all header fields in the vsock packet
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

    /// Get max number of bytes we can send to peer without overflowing
    /// the peer's buffer.
    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    /// Check if we need a credit update from the peer before sending
    /// more data to it.
    fn need_credit_update_from_peer(&self) -> bool {
        self.peer_avail_credit() == 0
    }
}
