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
        match self.peek() {
            Some(req) => {
                self.queue &= !req.clone().bitmask();
                Some(req)
            }
            None => None,
        }
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
            Some(RxOps::Rst)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains() {
        let mut rxqueue = RxQueue::new();
        rxqueue.queue = 31;

        assert_eq!(rxqueue.contains(RxOps::Request.bitmask()), true);
        assert_eq!(rxqueue.contains(RxOps::Rw.bitmask()), true);
        assert_eq!(rxqueue.contains(RxOps::Response.bitmask()), true);
        assert_eq!(rxqueue.contains(RxOps::CreditUpdate.bitmask()), true);
        assert_eq!(rxqueue.contains(RxOps::Rst.bitmask()), true);

        rxqueue.queue = 0;
        assert_eq!(rxqueue.contains(RxOps::Request.bitmask()), false);
        assert_eq!(rxqueue.contains(RxOps::Rw.bitmask()), false);
        assert_eq!(rxqueue.contains(RxOps::Response.bitmask()), false);
        assert_eq!(rxqueue.contains(RxOps::CreditUpdate.bitmask()), false);
        assert_eq!(rxqueue.contains(RxOps::Rst.bitmask()), false);
    }

    #[test]
    fn test_enqueue() {
        let mut rxqueue = RxQueue::new();

        rxqueue.enqueue(RxOps::Request);
        assert_eq!(rxqueue.contains(RxOps::Request.bitmask()), true);

        rxqueue.enqueue(RxOps::Rw);
        assert_eq!(rxqueue.contains(RxOps::Rw.bitmask()), true);

        rxqueue.enqueue(RxOps::Response);
        assert_eq!(rxqueue.contains(RxOps::Response.bitmask()), true);

        rxqueue.enqueue(RxOps::CreditUpdate);
        assert_eq!(rxqueue.contains(RxOps::CreditUpdate.bitmask()), true);

        rxqueue.enqueue(RxOps::Rst);
        assert_eq!(rxqueue.contains(RxOps::Rst.bitmask()), true);
    }

    #[test]
    fn test_peek() {
        let mut rxqueue = RxQueue::new();

        rxqueue.queue = 31;
        assert_eq!(rxqueue.peek(), Some(RxOps::Request));

        rxqueue.queue = 30;
        assert_eq!(rxqueue.peek(), Some(RxOps::Rw));

        rxqueue.queue = 28;
        assert_eq!(rxqueue.peek(), Some(RxOps::Response));

        rxqueue.queue = 24;
        assert_eq!(rxqueue.peek(), Some(RxOps::CreditUpdate));

        rxqueue.queue = 16;
        assert_eq!(rxqueue.peek(), Some(RxOps::Rst));
    }

    #[test]
    fn test_dequeue() {
        let mut rxqueue = RxQueue::new();
        rxqueue.queue = 31;

        assert_eq!(rxqueue.dequeue(), Some(RxOps::Request));
        assert_eq!(rxqueue.contains(RxOps::Request.bitmask()), false);

        assert_eq!(rxqueue.dequeue(), Some(RxOps::Rw));
        assert_eq!(rxqueue.contains(RxOps::Rw.bitmask()), false);

        assert_eq!(rxqueue.dequeue(), Some(RxOps::Response));
        assert_eq!(rxqueue.contains(RxOps::Response.bitmask()), false);

        assert_eq!(rxqueue.dequeue(), Some(RxOps::CreditUpdate));
        assert_eq!(rxqueue.contains(RxOps::CreditUpdate.bitmask()), false);

        assert_eq!(rxqueue.dequeue(), Some(RxOps::Rst));
        assert_eq!(rxqueue.contains(RxOps::Rst.bitmask()), false);
    }

    #[test]
    fn test_pending_rx() {
        let mut rxqueue = RxQueue::new();
        assert_eq!(rxqueue.pending_rx(), false);

        rxqueue.queue = 1;
        assert_eq!(rxqueue.pending_rx(), true);
    }
}
