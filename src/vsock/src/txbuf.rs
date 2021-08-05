#![deny(missing_docs)]

use std::io::Write;
use std::num::Wrapping;
use std::os::unix::net::UnixStream;

use super::Error;
use super::Result;
use super::CONN_TX_BUF_SIZE;

#[derive(Debug, Clone)]
pub struct LocalTxBuf {
    /// Buffer holding data to be forwarded to a host-side application
    buf: Vec<u8>,
    /// Index into buffer from which data can be consumed from the buffer
    head: Wrapping<u32>,
    /// Index into buffer from which data can be added to the buffer
    tail: Wrapping<u32>,
}

impl LocalTxBuf {
    /// Create a new instance of LocalTxBuf
    pub fn new() -> Self {
        Self {
            buf: vec![0; CONN_TX_BUF_SIZE as usize],
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    /// Check if the buf is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Add new data to the tx buffer, push all or none
    /// Returns LocalTxBufFull error if space not sufficient
    pub fn push(&mut self, data_buf: &[u8]) -> Result<()> {
        if CONN_TX_BUF_SIZE as usize - self.len() < data_buf.len() {
            // Tx buffer is full
            return Err(Error::LocalTxBufFull);
        }

        // Get index into buffer at which data can be inserted
        let tail_idx = self.tail.0 as usize % CONN_TX_BUF_SIZE as usize;

        // Check if we can fit the data buffer between head and end of buffer
        let len = std::cmp::min(CONN_TX_BUF_SIZE as usize - tail_idx, data_buf.len());
        self.buf[tail_idx..tail_idx + len].copy_from_slice(&data_buf[..len]);

        // Check if there is more data to be wrapped around
        if len < data_buf.len() {
            self.buf[..(data_buf.len() - len)].copy_from_slice(&data_buf[len..]);
        }

        // Increment tail by the amount of data that has been added to the buffer
        self.tail += Wrapping(data_buf.len() as u32);

        Ok(())
    }

    /// Flush buf data to stream
    pub fn flush_to(&mut self, stream: &mut UnixStream) -> Result<usize> {
        if self.is_empty() {
            // No data to be flushed
            return Ok(0);
        }

        // Get index into buffer from which data can be read
        let head_idx = self.head.0 as usize % CONN_TX_BUF_SIZE as usize;

        // First write from head to end of buffer
        let len = std::cmp::min(CONN_TX_BUF_SIZE as usize - head_idx, self.len());
        let written = stream
            .write(&self.buf[head_idx..(head_idx + len)])
            .map_err(Error::LocalTxBufFlush)?;

        // Increment head  by amount of data that has been flushed to the stream
        self.head += Wrapping(written as u32);

        // If written length is less than the expected length we can try again in the future
        if written < len {
            return Ok(written);
        }

        // The head index has wrapped around the end of the buffer, we call self again
        Ok(written + self.flush_to(stream).unwrap_or(0))
    }

    /// Return amount of data in the buffer
    fn len(&self) -> usize {
        (self.tail - self.head).0 as usize
    }
}
