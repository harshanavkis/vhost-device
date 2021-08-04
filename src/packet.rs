#![deny(missing_docs)]

use super::Error;
use super::Result;
use byteorder::ByteOrder;
use byteorder::LittleEndian;
use virtio_queue::DescriptorChain;
use vm_memory::GuestAddress;
use vm_memory::GuestAddressSpace;
use vm_memory::GuestMemory;
use vm_memory::GuestMemoryAtomic;
use vm_memory::GuestMemoryLoadGuard;
use vm_memory::GuestMemoryMmap;

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

/// Vsock packet structure implemented as a wrapper around a virtq descriptor chain:
/// - chain head holds the packet header
/// - optional data descriptor, only present for data packets (VSOCK_OP_RW)
pub struct VsockPacket {
    hdr: *mut u8,
    buf: Option<*mut u8>,
    buf_size: usize,
}

impl VsockPacket {
    /// Create a vsock packet wrapper around a chain in the rx virtqueue.
    /// Perform bounds checking before creating the wrapper.
    pub fn from_rx_virtq_head(
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

    // Create a vsock packet wrapper around a chain in the tx virtqueue
    // Bounds checking before creating the wrapper
    pub fn from_tx_virtq_head(
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
                // TODO: Turn this into an error
                break;
            }
            // let descr = chain.next().ok_or(Error::QueueMissingDescriptor)?;

            let descr = match chain.next() {
                Some(descr) => descr,
                None => break,
            };

            // All buffers in the tx queue must be readable only
            if descr.is_write_only() {
                return Err(Error::UnreadableDescriptor);
            }
            num_descr += 1;
            descr_vec.push(descr);
        }

        let head_descr = descr_vec[0];

        if head_descr.len() < VSOCK_PKT_HDR_SIZE as u32 {
            return Err(Error::HdrDescTooSmall(head_descr.len()));
        }

        let mut pkt = Self {
            hdr: VsockPacket::guest_to_host_address(
                &mem.memory(),
                head_descr.addr(),
                VSOCK_PKT_HDR_SIZE,
            )
            .ok_or(Error::GuestMemoryError)? as *mut u8,
            buf: None,
            buf_size: 0,
        };

        // Zero length packet
        if pkt.is_empty() {
            return Ok(pkt);
        }

        // TODO: Enforce a maximum packet size

        // There exists packet data as well
        let data_descr = descr_vec[1];

        // Data buffer should be as large as described in the header
        if data_descr.len() < pkt.len() {
            return Err(Error::DataDescTooSmall);
        }

        pkt.buf_size = data_descr.len() as usize;
        pkt.buf = Some(
            VsockPacket::guest_to_host_address(
                &mem.memory(),
                data_descr.addr(),
                data_descr.len() as usize,
            )
            .ok_or(Error::GuestMemoryError)? as *mut u8,
        );

        Ok(pkt)
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
    pub fn hdr(&self) -> &[u8] {
        // Safe as bound checks performed in from_*_virtq_head
        unsafe { std::slice::from_raw_parts(self.hdr as *const u8, VSOCK_PKT_HDR_SIZE) }
    }

    /// In place mutable slice access to vsock packet header
    pub fn hdr_mut(&mut self) -> &mut [u8] {
        // Safe as bound checks performed in from_*_virtq_head
        unsafe { std::slice::from_raw_parts_mut(self.hdr, VSOCK_PKT_HDR_SIZE) }
    }

    /// Size of vsock packet data, found by accessing len field
    /// of virtio_vsock_hdr struct
    pub fn len(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_LEN..])
    }

    /// Set the source cid
    pub fn set_src_cid(&mut self, cid: u64) -> &mut Self {
        LittleEndian::write_u64(&mut self.hdr_mut()[HDROFF_SRC_CID..], cid);
        self
    }

    /// Set the destination cid
    pub fn set_dst_cid(&mut self, cid: u64) -> &mut Self {
        LittleEndian::write_u64(&mut self.hdr_mut()[HDROFF_DST_CID..], cid);
        self
    }

    /// Set source port
    pub fn set_src_port(&mut self, port: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_SRC_PORT..], port);
        self
    }

    /// Set destination port
    pub fn set_dst_port(&mut self, port: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_DST_PORT..], port);
        self
    }

    /// Set type of connection
    pub fn set_type(&mut self, type_: u16) -> &mut Self {
        LittleEndian::write_u16(&mut self.hdr_mut()[HDROFF_TYPE..], type_);
        self
    }

    /// Set size of tx buf
    pub fn set_buf_alloc(&mut self, buf_alloc: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_BUF_ALLOC..], buf_alloc);
        self
    }

    /// Set amount of tx buf data written to stream
    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_FWD_CNT..], fwd_cnt);
        self
    }

    /// Set packet operation ID
    pub fn set_op(&mut self, op: u16) -> &mut Self {
        LittleEndian::write_u16(&mut self.hdr_mut()[HDROFF_OP..], op);
        self
    }

    /// Check if the packet has no data
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get destination port from packet
    pub fn dst_port(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_DST_PORT..])
    }

    /// Get source port from packet
    pub fn src_port(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_SRC_PORT..])
    }

    /// Get source cid from packet
    pub fn src_cid(&self) -> u64 {
        LittleEndian::read_u64(&self.hdr()[HDROFF_SRC_CID..])
    }

    /// Get destination cid from packet
    pub fn dst_cid(&self) -> u64 {
        LittleEndian::read_u64(&self.hdr()[HDROFF_DST_CID..])
    }

    /// Get packet type
    pub fn pkt_type(&self) -> u16 {
        LittleEndian::read_u16(&self.hdr()[HDROFF_TYPE..])
    }

    /// Get operation requested in the packet
    pub fn op(&self) -> u16 {
        LittleEndian::read_u16(&self.hdr()[HDROFF_OP..])
    }

    /// Byte slice mutable access to vsock packet data buffer
    pub fn buf_mut(&mut self) -> Option<&mut [u8]> {
        // Safe as bound checks performed while creating packet
        self.buf
            .map(|ptr| unsafe { std::slice::from_raw_parts_mut(ptr, self.buf_size) })
    }

    /// Byte slice access to vsock packet data buffer
    pub fn buf(&self) -> Option<&[u8]> {
        // Safe as bound checks performed while creating packet
        self.buf
            .map(|ptr| unsafe { std::slice::from_raw_parts(ptr as *const u8, self.buf_size) })
    }

    /// Set data buffer length
    pub fn set_len(&mut self, len: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_LEN..], len);
        self
    }

    /// Read buf alloc
    pub fn buf_alloc(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_BUF_ALLOC..])
    }

    /// Get fwd cnt from packet header
    pub fn fwd_cnt(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_FWD_CNT..])
    }

    /// Read flags from the packet header
    pub fn flags(&self) -> u32 {
        LittleEndian::read_u32(&self.hdr()[HDROFF_FLAGS..])
    }

    /// Set packet header flag to flags
    pub fn set_flags(&mut self, flags: u32) -> &mut Self {
        LittleEndian::write_u32(&mut self.hdr_mut()[HDROFF_FLAGS..], flags);
        self
    }

    /// Set OP specific flags
    pub fn set_flag(&mut self, flag: u32) -> &mut Self {
        self.set_flags(self.flags() | flag);
        self
    }
}
