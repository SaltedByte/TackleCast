//! Lock-free single-producer single-consumer triple buffer.
//!
//! The producer fills its "back" slot and publishes it atomically. The
//! consumer refreshes its "front" slot from the latest published value and
//! borrows it in place.
//!
//! Memory is bounded to exactly 3 instances of `T`, and no heap allocation
//! occurs after construction. Values are never moved out of their slots, so
//! each slot keeps its allocations (a frame's `Vec` capacity, say) and cycles
//! them back to the producer to be filled again.
//!
//! # Safety
//!
//! Each slot is accessed by at most one thread at a time:
//! - The producer exclusively owns its back slot.
//! - The consumer exclusively owns its front slot.
//! - The ready slot is in transit (no one accesses its contents).
//!
//! The three indices are always distinct, and ownership transfers only through
//! `swap` on `ready`, so the invariant holds under any interleaving.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Bit 2 of the ready byte: set when the producer has published since the
/// consumer's last read.
const DIRTY_BIT: u8 = 0x04;

/// Bits 0-1 of the ready byte: the slot index.
const INDEX_MASK: u8 = 0x03;

/// Shared state between producer and consumer.
struct Shared<T> {
    slots: [UnsafeCell<T>; 3],
    /// Packed: bits 0-1 = ready slot index (0..2), bit 2 = dirty flag.
    ready: AtomicU8,
}

// Safety: `T: Send` lets us transfer T between threads, and the atomic ready
// index guarantees no two threads ever access the same slot concurrently.
unsafe impl<T: Send> Sync for Shared<T> {}

/// Producer half — fill `back_slot()` then `publish()`, or just `write()`.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    back_index: u8,
}

/// Consumer half — call `read()` to borrow the latest published value.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    front_index: u8,
}

unsafe impl<T: Send> Send for Producer<T> {}
unsafe impl<T: Send> Send for Consumer<T> {}

/// Create a new triple buffer, returning the producer and consumer halves.
///
/// `init` is called three times to populate the slots. For frame buffers, pass
/// a closure that creates an empty frame; the slots grow to the working size on
/// the first few frames and then stay there.
pub fn triple_buffer<T, F>(mut init: F) -> (Producer<T>, Consumer<T>)
where
    F: FnMut() -> T,
{
    let shared = Arc::new(Shared {
        slots: [
            UnsafeCell::new(init()),
            UnsafeCell::new(init()),
            UnsafeCell::new(init()),
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
    /// Borrow the back slot to fill in place.
    ///
    /// The slot still holds the previous value that occupied it, so its
    /// allocations can be reused rather than reallocated.
    ///
    /// # Safety guarantee
    ///
    /// The producer exclusively owns `back_index` — no other thread can access
    /// this slot until `publish()` moves it to the ready position.
    pub fn back_slot(&mut self) -> &mut T {
        // Safety: the producer exclusively owns back_index's slot.
        unsafe { &mut *self.shared.slots[self.back_index as usize].get() }
    }

    /// Publish the back slot as the new ready value and reclaim the old ready
    /// slot as the new back slot.
    ///
    /// Wait-free: a single atomic swap.
    pub fn publish(&mut self) {
        let old_ready = self
            .shared
            .ready
            .swap(self.back_index | DIRTY_BIT, Ordering::AcqRel);
        // The old ready slot becomes our new back slot.
        self.back_index = old_ready & INDEX_MASK;
    }

    /// Convenience: overwrite the back slot and publish it.
    ///
    /// This discards whatever the slot was holding. Prefer filling
    /// `back_slot()` in place when the value owns heap allocations worth
    /// reusing.
    pub fn write(&mut self, value: T) {
        *self.back_slot() = value;
        self.publish();
    }
}

impl<T> Consumer<T> {
    /// Refresh the front slot from the latest published value and borrow it.
    ///
    /// Returns `None` when the producer has published nothing new since the
    /// last call. Intermediate values are skipped — the consumer always jumps
    /// to the newest.
    ///
    /// The value stays in its slot, so its allocations travel back to the
    /// producer the next time the slots rotate.
    pub fn read(&mut self) -> Option<&T> {
        // Nothing new to pick up.
        if self.shared.ready.load(Ordering::Acquire) & DIRTY_BIT == 0 {
            return None;
        }

        // Hand our front slot over (clean, so the producer's next publish sees
        // no stale dirty bit) and claim whatever was in the ready position.
        let old_ready = self.shared.ready.swap(self.front_index, Ordering::AcqRel);
        self.front_index = old_ready & INDEX_MASK;

        // Safety: the slot we just claimed was the ready slot, and our previous
        // front slot now sits in the ready position, so the producer cannot be
        // touching this one until we swap it back.
        Some(unsafe { &*self.shared.slots[self.front_index as usize].get() })
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
        assert_eq!(consumer.read(), Some(&42));
        assert!(consumer.read().is_none());
    }

    #[test]
    fn multiple_writes_before_read_returns_latest() {
        let (mut producer, mut consumer) = triple_buffer(|| 0_u32);

        producer.write(1);
        producer.write(2);
        producer.write(3);

        // Consumer should get the latest value (3), intermediate values are lost
        assert_eq!(consumer.read(), Some(&3));
        assert!(consumer.read().is_none());
    }

    /// The reason this type exists rather than a channel: every slot that
    /// reaches the producer must still carry the allocation it had before, so
    /// steady-state frame decoding never hits the allocator.
    ///
    /// Mirrors the real pipeline — the producer fills the back slot in place
    /// (as the decoder does) and the consumer borrows without taking (as the
    /// renderer does).
    #[test]
    fn slots_retain_capacity_across_many_cycles() {
        const CAP: usize = 4096;
        let (mut producer, mut consumer) = triple_buffer(|| Vec::<u8>::with_capacity(CAP));

        for round in 0..20 {
            let buf = producer.back_slot();
            assert!(
                buf.capacity() >= CAP,
                "back slot arrived without its allocation on round {round} \
                 (capacity {}) — frames would reallocate every time",
                buf.capacity()
            );
            buf.clear();
            buf.resize(CAP, 7);
            producer.publish();

            let frame = consumer.read().expect("a value was just published");
            assert_eq!(frame.len(), CAP);
        }
    }

    /// The safety argument rests entirely on back, ready, and front always
    /// being three different slots.
    #[test]
    fn slot_indices_stay_distinct() {
        let (mut producer, mut consumer) = triple_buffer(|| 0_u32);

        for i in 0..50_u32 {
            producer.write(i);

            let ready = producer.shared.ready.load(Ordering::Relaxed) & INDEX_MASK;
            assert_ne!(producer.back_index, ready, "back aliased ready at {i}");
            assert_ne!(consumer.front_index, ready, "front aliased ready at {i}");
            assert_ne!(
                producer.back_index, consumer.front_index,
                "back aliased front at {i}"
            );

            // Read on an uneven cadence so the two halves interleave.
            if i % 3 == 0 {
                let _ = consumer.read();
            }
        }
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
            if let Some(&val) = consumer.read() {
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
