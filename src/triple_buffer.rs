//! Lock-free single-producer single-consumer triple buffer.
//!
//! The producer writes into its "back" slot and publishes it atomically.
//! The consumer reads from its "front" slot and refreshes it from the
//! latest published value when desired.
//!
//! Memory is bounded to exactly 3 instances of `T`. No heap allocation
//! occurs after construction. The consumer returns its previous front slot
//! back to the producer, enabling allocation reuse (e.g., recycling `Vec`
//! capacity across frames).
//!
//! # Safety
//!
//! Each slot is accessed by at most one thread at a time:
//! - The producer exclusively owns its back slot.
//! - The consumer exclusively owns its front slot.
//! - The ready slot is in transit (no one accesses its contents).
//!
//! Ownership transfers atomically via the `ready` index.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Bit 2 of the ready byte: set when producer has published since last consumer read.
const DIRTY_BIT: u8 = 0x04;

/// Shared state between producer and consumer.
struct Shared<T> {
    slots: [UnsafeCell<Option<T>>; 3],
    /// Packed: bits 0-1 = ready slot index (0..2), bit 2 = dirty flag.
    ready: AtomicU8,
}

// Safety: T: Send means we can transfer T between threads. The atomic ready
// index ensures exclusive access to each slot — no two threads access the
// same slot concurrently.
unsafe impl<T: Send> Sync for Shared<T> {}

/// Producer half — call `write()` to publish a new value.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    back_index: u8,
}

/// Consumer half — call `read()` to get the latest published value.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    front_index: u8,
}

unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

/// Create a new triple buffer, returning the producer and consumer halves.
///
/// `init` is called three times to populate the initial slots. For frame
/// buffers, pass a closure that creates an empty/default frame with
/// pre-allocated capacity.
pub fn triple_buffer<T, F>(mut init: F) -> (Producer<T>, Consumer<T>)
where
    F: FnMut() -> T,
{
    let shared = Arc::new(Shared {
        slots: [
            UnsafeCell::new(Some(init())),
            UnsafeCell::new(Some(init())),
            UnsafeCell::new(Some(init())),
        ],
        // Initial: ready index = 1, not dirty
        ready: AtomicU8::new(1),
    });

    let producer = Producer {
        shared: shared.clone(),
        back_index: 0,
    };

    let consumer = Consumer {
        shared,
        front_index: 2,
    };

    (producer, consumer)
}

impl<T> Producer<T> {
    /// Get a mutable reference to the back slot for in-place writing.
    ///
    /// The returned reference allows the producer to reuse the previous
    /// value's allocations (e.g., writing into an existing `Vec` without
    /// reallocating).
    ///
    /// # Safety guarantee
    ///
    /// The producer exclusively owns `back_index` — no other thread can
    /// access this slot until `publish()` transfers it to the ready position.
    pub fn back_slot(&mut self) -> &mut Option<T> {
        // Safety: producer exclusively owns back_index's slot
        unsafe { &mut *self.shared.slots[self.back_index as usize].get() }
    }

    /// Publish the current back slot as the new ready value and reclaim
    /// the old ready slot as the new back slot.
    ///
    /// This is wait-free: a single atomic swap.
    pub fn publish(&mut self) {
        let new_ready = self.back_index | DIRTY_BIT;
        let old_ready = self.shared.ready.swap(new_ready, Ordering::AcqRel);
        // The old ready slot becomes our new back slot
        self.back_index = old_ready & 0x03;
    }

    /// Convenience: write a value into the back slot and publish it.
    pub fn write(&mut self, value: T) {
        *self.back_slot() = Some(value);
        self.publish();
    }
}

impl<T> Consumer<T> {
    /// Take the latest published value, returning `Some(T)` if a new value
    /// was available since the last read, or `None` if the consumer is
    /// already up-to-date.
    ///
    /// The consumer's previous front slot is returned to the producer for
    /// reuse (allocation recycling).
    pub fn read(&mut self) -> Option<T> {
        // Only swap if dirty
        let current = self.shared.ready.load(Ordering::Acquire);
        if current & DIRTY_BIT == 0 {
            return None;
        }

        // Swap our front index into the ready slot (clearing dirty flag)
        let new_ready = self.front_index; // no dirty bit = clean
        let old_ready = self.shared.ready.swap(new_ready, Ordering::AcqRel);

        let new_front_index = old_ready & 0x03;
        self.front_index = new_front_index;

        // Safety: we now exclusively own new_front_index (it was the ready
        // slot, and we've swapped our old front into ready — no one else
        // will touch new_front_index until we swap it back).
        

        unsafe { (*self.shared.slots[new_front_index as usize].get()).take() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read() {
        let (mut producer, mut consumer) = triple_buffer(|| 0_u32);

        assert!(consumer.read().is_none());

        producer.write(42);
        assert_eq!(consumer.read(), Some(42));
        assert!(consumer.read().is_none());
    }

    #[test]
    fn multiple_writes_before_read_returns_latest() {
        let (mut producer, mut consumer) = triple_buffer(|| 0_u32);

        producer.write(1);
        producer.write(2);
        producer.write(3);

        // Consumer should get the latest value (3), intermediate values are lost
        assert_eq!(consumer.read(), Some(3));
        assert!(consumer.read().is_none());
    }

    #[test]
    fn recycling_preserves_capacity() {
        let (mut producer, mut consumer) = triple_buffer(|| Vec::<u8>::with_capacity(1024));

        // Write a value using the pre-allocated back slot
        {
            let slot = producer.back_slot();
            let buf = slot.as_mut().unwrap();
            buf.clear();
            buf.extend_from_slice(&[1, 2, 3]);
        }
        producer.publish();

        // Consumer reads
        let frame = consumer.read().unwrap();
        assert_eq!(frame, vec![1, 2, 3]);

        // Publish again so the consumer's old front slot cycles back to producer
        producer.write(Vec::new());
        let _ = consumer.read();

        // Producer's back slot should have capacity from the recycled initial slot
        let slot = producer.back_slot();
        let buf = slot.as_ref().unwrap();
        assert!(buf.capacity() >= 1024);
    }

    #[test]
    fn cross_thread_monotonic() {
        let (mut producer, mut consumer) = triple_buffer(|| 0_u64);

        let handle = std::thread::spawn(move || {
            for i in 0..10_000 {
                producer.write(i);
                // Occasional yield to let consumer run
                if i % 100 == 0 {
                    std::thread::yield_now();
                }
            }
        });

        let mut last_seen = None;
        loop {
            if let Some(val) = consumer.read() {
                // Values should be monotonically non-decreasing
                if let Some(prev) = last_seen {
                    assert!(val >= prev, "got {val} after {prev}");
                }
                last_seen = Some(val);
                if val >= 9_999 {
                    break;
                }
            }
            std::thread::yield_now();
        }

        handle.join().unwrap();
    }
}
