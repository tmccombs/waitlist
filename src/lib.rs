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
const SENTINEL: usize = usize::max_value();

pub struct Waitlist {
    flags: AtomicUsize,
    inner: UnsafeCell<Inner>,
}

pub struct WaitRef<'a> {
    waitlist: &'a Waitlist,
    index: usize,
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
                next_waiting: SENTINEL,
                last_waiting: SENTINEL,
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
    pub fn insert(&self, cx: &Context) -> WaitRef<'_> {
        let waker = cx.waker().clone();
        let idx = self.lock().add_waker(waker);
        WaitRef {
            waitlist: self,
            index: idx,
        }
    }

    // FIXME: figure out a better name for this
    #[inline]
    pub fn remove_if_notified(&self, key: usize, cx: &Context<'_>) -> bool {
        self.lock().remove_if_notified(key, cx)
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

impl<'a> WaitRef<'a> {
    #[inline]
    pub fn update(&self, cx: &Context) {
        let waker = cx.waker().clone();
        self.waitlist.lock().update(self.index, waker)
    }

    /// Remove waker associated with this reference from the
    /// waitlist without triggering another notify.
    #[inline]
    pub fn remove(self) -> bool {
        let was_notified = self.waitlist.lock().remove(self.index);
        mem::forget(self); // forget self, because the default is a cancel
        was_notified
    }

    #[inline]
    pub fn cancel(mut self) -> bool {
        let did_notify = self.cancel_inner();
        mem::forget(self); // forget so we don't try removing it again
        did_notify
    }

    fn cancel_inner(&mut self) -> bool {
        let mut inner = self.waitlist.lock();
        if inner.remove(self.index) {
            inner.notify_first()
        } else {
            false
        }
    }
}

impl<'a> Drop for WaitRef<'a> {
    #[inline]
    fn drop(&mut self) {
        // by default dropping a WaitRef will cancel,
        // triggering another waker if necessary
        self.cancel_inner();
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
            next: SENTINEL,
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
                    next: SENTINEL,
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

    #[cold]
    fn remove(&mut self, key: usize) -> bool {
        let entry = self.take_slot(key);
        match entry {
            Slot::Waiting {
                next: next_slot, ..
            } => {
                self.waiting_count -= 1;
                if key == self.next_waiting {
                    self.next_waiting = next_slot;
                } else {
                    let mut current = self.next_waiting;
                    while current != SENTINEL {
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
                false
            }
            Slot::Notified => {
                self.notified_count -= 1;
                true
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
        if self.next_waiting != SENTINEL {
            self.next_waiting = self.notify_slot(self.next_waiting);
            self.waiting_count -= 1;
            self.notified_count += 1;
            true
        } else {
            false
        }
    }

    fn notify_all(&mut self) -> bool {
        let notified = self.next_waiting != SENTINEL;
        let mut current = self.next_waiting;
        while current != SENTINEL {
            current = self.notify_slot(current);
        }
        self.notified_count += self.waiting_count;
        self.waiting_count = 0;
        self.next_waiting = SENTINEL;
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
        if self.last_waiting == SENTINEL {
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

    fn add_all<'a>(wl: &'a Waitlist, wakers: &[MockWaker]) -> Vec<WaitRef<'a>> {
        wakers.iter().map(|w| wl.insert(&w.to_context())).collect()
    }

    #[test]
    fn fifo_order() {
        const N: usize = 7;
        let wakers: [MockWaker; N] = Default::default();
        let waitlist = Waitlist::new();
        let _refs = add_all(&waitlist, &wakers);

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

    #[test]
    fn notify_all() {
        const N: usize = 7;
        let wakers: [MockWaker; N] = Default::default();
        let waitlist = Waitlist::new();
        let _refs = add_all(&waitlist, &wakers);
        waitlist.notify_all();

        for (i, w) in wakers.iter().enumerate() {
            assert_eq!(1, w.notified_count(), "Waker {} was not notified", i);
        }
    }

    #[test]
    fn cancel_notifies_next() {
        let w1 = MockWaker::new();
        let w2 = MockWaker::new();
        let waitlist = Waitlist::new();

        let k1 = waitlist.insert(&w1.to_context());
        let _k2 = waitlist.insert(&w2.to_context());

        waitlist.notify_one();
        assert!(k1.cancel());
        assert_eq!(1, w2.notified_count(), "Second task wasn't notified");
    }
}
