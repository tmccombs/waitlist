use core::cell::UnsafeCell;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Waker};

use crossbeam_utils::Backoff;

extern crate alloc;
use alloc::vec::Vec;

enum Slot {
    Available { next: usize },
    Waiting { next: usize, waker: Waker },
    Notified,
}

struct Inner {
    queue: Vec<Slot>,
    waiting_count: usize,
    notified_count: usize,
    next_avail: usize,
    next_waiting: usize,
    last_waiting: usize,
}

// Set when the queue is locked.
const LOCKED: usize = 1;

// Set when there is at least one notifiable waker
const WAITING: usize = 1 << 1;

// Set when we at least one task has been notified, but hasn't
// yet been removed
const NOTIFIED: usize = 1 << 2;

// Index for the `next` value of the tail of the
// waiting queue
const NULL_INDEX: usize = usize::max_value();

pub struct Waitlist {
    flags: AtomicUsize,
    inner: UnsafeCell<Inner>,
}

impl Waitlist {
    #[inline]
    pub fn new() -> Waitlist {
        Self::with_capacity(0)
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> Waitlist {
        Waitlist {
            flags: AtomicUsize::new(0),
            inner: UnsafeCell::new(Inner {
                queue: Vec::with_capacity(cap),
                waiting_count: 0,
                notified_count: 0,
                next_avail: 0,
                next_waiting: NULL_INDEX,
                last_waiting: NULL_INDEX,
            }),
        }
    }

    fn lock(&self) -> Guard<'_> {
        let backoff = Backoff::new();
        while self.flags.fetch_or(LOCKED, Ordering::Acquire) & LOCKED != 0 {
            backoff.snooze();
        }
        Guard { waitlist: self }
    }

    #[inline]
    pub fn insert(&self, cx: &Context) -> usize {
        let waker = cx.waker().clone();
        self.lock().add_waker(waker)
    }

    #[inline]
    pub fn update(&self, key: usize, cx: &Context) {
        let waker = cx.waker().clone();
        self.lock().update(key, waker)
    }

    #[inline]
    pub fn remove(&self, key: usize) -> bool {
        self.lock().remove(key, false)
    }

    // FIXME: figure out a better name for this
    #[inline]
    pub fn remove_if_notified(&self, key: usize, cx: &Context<'_>) -> bool {
        self.lock().remove_if_notified(key, cx)
    }

    #[inline]
    pub fn cancel(&self, key: usize) -> bool {
        //FIXME the return value here doesn't match the waker_set from async-std,
        //figure out a better way to handle this
        self.lock().remove(key, true)
    }

    #[inline]
    pub fn notify_one(&self) -> bool {
        if self.flags.load(Ordering::SeqCst) & WAITING != 0 {
            self.lock().notify_first()
        } else {
            false
        }
    }

    #[inline]
    pub fn notify_all(&self) -> bool {
        if self.flags.load(Ordering::SeqCst) & WAITING != 0 {
            self.lock().notify_all()
        } else {
            false
        }
    }

    #[inline]
    pub fn notify_any(&self) -> bool {
        let flags = self.flags.load(Ordering::SeqCst);
        if flags & NOTIFIED == 0 && flags & WAITING != 0 {
            let mut inner = self.lock();
            // We need check the notified_count, because
            // the number of notified tasks may have changed
            // between checking the flags and getting the lock
            if inner.notified_count > 0 {
                inner.notify_first()
            } else {
                false
            }
        } else {
            false
        }
    }
}

unsafe impl Send for Waitlist {}
unsafe impl Sync for Waitlist {}

impl Default for Waitlist {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    fn add_waker(&mut self, waker: Waker) -> usize {
        let key = self.next_avail;
        let entry = Slot::Waiting {
            next: self.next_waiting,
            waker,
        };
        if key == self.queue.len() {
            self.queue.push(entry);
            self.next_avail += 1;
        } else {
            self.next_avail = match self.queue.get(key) {
                Some(&Slot::Available { next }) => next,
                _ => unreachable!(),
            };
            self.queue[key] = entry;
        }
        self.queue_slot(key);
        key
    }

    fn update(&mut self, key: usize, w: Waker) {
        let was_notified = match self.queue.get_mut(key) {
            Some(slot @ &mut Slot::Notified) => {
                *slot = Slot::Waiting {
                    next: NULL_INDEX,
                    waker: w,
                };
                true
            }
            Some(&mut Slot::Waiting { ref mut waker, .. }) => {
                *waker = w;
                false
            }
            _ => unreachable!(),
        };
        if was_notified {
            // if the slot was already notified, add the slot back to the end of the
            // wait queue
            self.queue_slot(key);
            self.notified_count -= 1;
        }
    }

    #[inline]
    fn remove(&mut self, key: usize, cancel: bool) -> bool {
        let entry = self.take_slot(key);
        match entry {
            Slot::Waiting {
                next: next_slot, ..
            } => {
                self.waiting_count -= 1;
                if key == self.next_avail {
                    self.next_avail = next_slot;
                } else {
                    let mut current = self.next_avail;
                    while current != NULL_INDEX {
                        if let Slot::Waiting { ref mut next, .. } = self.queue[key] {
                            if *next == key {
                                *next = next_slot;
                                break;
                            } else {
                                current = *next;
                            }
                        } else {
                            unreachable!();
                        }
                    }
                }
                true
            }
            Slot::Notified => {
                self.notified_count -= 1;
                if cancel {
                    // If cancelling, and we were notified, wake up the next task
                    // in the queue
                    self.notify_first();
                }
                false
            }
            Slot::Available { .. } => unreachable!("attempt to remove non-existant entry"),
        }
    }

    fn remove_if_notified(&mut self, key: usize, cx: &Context<'_>) -> bool {
        let should_remove = match self.queue[key] {
            Slot::Waiting { ref mut waker, .. } => {
                *waker = cx.waker().clone();
                false
            }
            Slot::Notified => true,
            Slot::Available { .. } => unreachable!("Attempt to remove non-existant entry"),
        };
        if should_remove {
            self.queue[key] = Slot::Available {
                next: self.next_avail,
            };
            self.next_avail = key;
            self.notified_count -= 1;
        }
        should_remove
    }

    fn notify_first(&mut self) -> bool {
        if self.next_waiting != NULL_INDEX {
            self.next_waiting = self.notify_slot(self.next_waiting);
            self.waiting_count -= 1;
            self.notified_count += 1;
            true
        } else {
            false
        }
    }

    fn notify_all(&mut self) -> bool {
        let notified = self.next_waiting != NULL_INDEX;
        let mut current = self.next_waiting;
        while current != NULL_INDEX {
            current = self.notify_slot(current);
        }
        self.notified_count += self.waiting_count;
        self.waiting_count = 0;
        self.next_waiting = NULL_INDEX;
        notified
    }

    fn notify_slot(&mut self, key: usize) -> usize {
        if let Slot::Waiting { next, waker } = mem::replace(&mut self.queue[key], Slot::Notified) {
            waker.wake();
            next
        } else {
            unreachable!()
        }
    }

    fn take_slot(&mut self, key: usize) -> Slot {
        let slot = mem::replace(
            &mut self.queue[key],
            Slot::Available {
                next: self.next_avail,
            },
        );
        self.next_avail = key;
        slot
    }

    fn queue_slot(&mut self, key: usize) {
        if self.last_waiting == NULL_INDEX {
            // This is the first waiter, so update both ends of the queue
            self.next_waiting = key;
            self.last_waiting = key;
        } else if let Slot::Waiting { ref mut next, .. } = self.queue[self.last_waiting] {
            *next = key;
            self.last_waiting = key;
        } else {
            unreachable!();
        }
        self.waiting_count += 1;
    }
}

struct Guard<'a> {
    waitlist: &'a Waitlist,
}

impl<'a> Deref for Guard<'a> {
    type Target = Inner;

    #[inline]
    fn deref(&self) -> &Inner {
        unsafe { &*self.waitlist.inner.get() }
    }
}

impl<'a> DerefMut for Guard<'a> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Inner {
        unsafe { &mut *self.waitlist.inner.get() }
    }
}

impl<'a> Drop for Guard<'a> {
    fn drop(&mut self) {
        let mut flags = 0;

        if self.waiting_count > 0 {
            flags |= WAITING;
        }

        if self.notified_count > 0 {
            flags |= NOTIFIED;
        }

        // Synchronise with `lock()`.
        self.waitlist.flags.store(flags, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod mock_waker;

#[cfg(test)]
mod tests {
    use super::mock_waker::MockWaker;
    use super::*;

    #[test]
    fn fifo_order() {
        const N: usize = 7;
        let wakers: [MockWaker; N] = Default::default();
        let waitlist = Waitlist::new();
        for waker in wakers.iter() {
            waitlist.insert(&waker.to_context());
        }

        for i in 0..N {
            waitlist.notify_one();

            for (j, w) in wakers.iter().enumerate() {
                let expected = if j <= i { 1 } else { 0 };
                assert_eq!(
                    expected,
                    w.notified_count(),
                    "Incorrect notification count for waker {} after notification {}",
                    j,
                    i
                );
            }
        }
    }
}
