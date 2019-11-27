use core::cell::UnsafeCell;
use core::future::Future;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

extern crate alloc;
use alloc::fmt;
use alloc::vec::Vec;

use crossbeam_utils::Backoff;

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

/**
 */
pub struct Waitlist {
    flags: AtomicUsize,
    inner: UnsafeCell<Inner>,
}

pub struct WaitRef<'a> {
    waitlist: &'a Waitlist,
    index: usize,
}

pub struct WaitFuture<'a> {
    waitlist: &'a Waitlist,
    key: Option<usize>,
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

    #[inline]
    pub fn wait(&self) -> WaitFuture<'_> {
        WaitFuture {
            waitlist: self,
            key: None,
        }
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
            if inner.notified_count == 0 {
                inner.notify_first()
            } else {
                false
            }
        } else {
            false
        }
    }
}

impl fmt::Debug for Waitlist {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Waitlist")
            .field("flags", &self.flags)
            .finish()
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
    pub fn cancel(self) -> bool {
        let did_notify = self.waitlist.lock().cancel(self.index);
        mem::forget(self); // forget so we don't try removing it again
        did_notify
    }
}

impl<'a> Drop for WaitRef<'a> {
    #[inline]
    fn drop(&mut self) {
        // by default dropping a WaitRef will cancel,
        // triggering another waker if necessary
        self.waitlist.lock().cancel(self.index);
    }
}

impl<'a> Future for WaitFuture<'a> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(key) = self.key {
            if self.waitlist.lock().remove_if_notified(key, cx) {
                self.key = None;
                return Poll::Ready(());
            }
        } else {
            let waker = cx.waker().clone();
            self.key = Some(self.waitlist.lock().add_waker(waker));
        }
        Poll::Pending
    }
}

impl<'a> Drop for WaitFuture<'a> {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            self.waitlist.lock().cancel(key);
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
                    // find and update the previous entry in the queue
                    while current != SENTINEL {
                        if let Slot::Waiting { ref mut next, .. } = self.queue[current] {
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

    fn cancel(&mut self, key: usize) -> bool {
        if self.remove(key) {
            self.notify_first()
        } else {
            false
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
        let key = self.next_waiting;
        if key != SENTINEL {
            self.next_waiting = self.notify_slot(key);
            self.waiting_count -= 1;
            self.notified_count += 1;
            if self.next_waiting == SENTINEL {
                self.last_waiting = SENTINEL;
            }
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
        self.last_waiting = SENTINEL;
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
