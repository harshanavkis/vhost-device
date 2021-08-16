#![deny(missing_docs)]

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RxOps {
    /// VSOCK_OP_REQUEST
    Request = 0,
    /// VSOCK_OP_RW
    Rw = 1,
    /// VSOCK_OP_RESPONSE
    Response = 2,
    /// VSOCK_OP_CREDIT_UPDATE
    CreditUpdate = 3,
    /// VSOCK_OP_RST
    Reset = 4,
}

impl RxOps {
    /// Convert enum value into bitmask
    pub fn bitmask(self) -> u8 {
        1u8 << (self as u8)
    }
}
