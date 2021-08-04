#![deny(missing_docs)]
use super::RxOps;

#[derive(Debug)]
pub struct RxQueue {
    /// bitmap of rx operations
    queue: u8,
}

impl RxQueue {
    /// new instance of RxQueue
    pub fn new() -> Self {
        RxQueue { queue: 0 as u8 }
    }

    /// Enqueue a new rx operation into the queue
    pub fn enqueue(&mut self, op: RxOps) {
        self.queue |= op.bitmask();
    }

    /// Dequeue an rx operation from the queue
    pub fn dequeue(&mut self) -> Option<RxOps> {
        let op = match self.peek() {
            Some(req) => {
                self.queue = self.queue & (!req.clone().bitmask());
                Some(req)
            }
            None => None,
        };

        op
    }

    /// Peek into the queue to check if it contains an rx operation
    pub fn peek(&self) -> Option<RxOps> {
        if self.contains(RxOps::Request.bitmask()) {
            return Some(RxOps::Request);
        }
        if self.contains(RxOps::Rw.bitmask()) {
            return Some(RxOps::Rw);
        }
        if self.contains(RxOps::Response.bitmask()) {
            return Some(RxOps::Response);
        }
        if self.contains(RxOps::CreditUpdate.bitmask()) {
            return Some(RxOps::CreditUpdate);
        }
        if self.contains(RxOps::Rst.bitmask()) {
            return Some(RxOps::Rst);
        } else {
            None
        }
    }

    /// Check if the queue contains a particular rx operation
    pub fn contains(&self, op: u8) -> bool {
        (self.queue & op) != 0
    }

    /// Check if there are any pending rx operations in the queue
    pub fn pending_rx(&self) -> bool {
        self.queue != 0
    }
}
