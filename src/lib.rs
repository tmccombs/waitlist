use core::cell::UnsafeCell;
use core::future::Future;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

extern crate alloc;
use alloc::collections::vec_deque::VecDeque;
use alloc::fmt;

use crossbeam_utils::Backoff;

struct Waiter {
    key: usize,
    waker: Waker,
}

struct Inner {
    queue: VecDeque<Waiter>,
    notified_count: usize,
    min_key: usize,
    next_key: usize,
}

// Set when the queue is locked.
const LOCKED: usize = 1;

// Set when there is at least one notifiable waker
const WAITING: usize = 1 << 1;

// Set when we at least one task has been notified, but hasn't
// yet been removed
const NOTIFIED: usize = 1 << 2;

/**
 */
pub struct Waitlist {
    flags: AtomicUsize,
    inner: UnsafeCell<Inner>,
}

pub struct WaitRef<'a> {
    waitlist: &'a Waitlist,
    key: usize,
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
                queue: VecDeque::with_capacity(cap),
                notified_count: 0,
                min_key: 0,
                next_key: 0,
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
        let key = self.lock().insert(waker);
        WaitRef {
            waitlist: self,
            key,
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
    pub fn update(&mut self, cx: &Context) {
        let waker = cx.waker().clone();
        self.key = self.waitlist.lock().update(self.key, waker);
    }

    /// Remove waker associated with this reference from the
    /// waitlist without triggering another notify.
    #[inline]
    pub fn remove(self) -> bool {
        let was_notified = self.waitlist.lock().remove(self.key);
        mem::forget(self); // forget self, because the default is a cancel
        was_notified
    }

    #[inline]
    pub fn cancel(self) -> bool {
        let did_notify = self.waitlist.lock().cancel(self.key);
        mem::forget(self); // forget so we don't try removing it again
        did_notify
    }
}

impl<'a> Drop for WaitRef<'a> {
    #[inline]
    fn drop(&mut self) {
        // by default dropping a WaitRef will cancel,
        // triggering another waker if necessary
        self.waitlist.lock().cancel(self.key);
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
            self.key = Some(self.waitlist.lock().insert(waker));
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
    fn is_in_waiting_range(&self, key: usize) -> bool {
        // the part after `||` is to deal with if the key wraps around
        key >= self.min_key || (self.next_key < self.min_key && key < self.next_key)
    }

    fn insert(&mut self, waker: Waker) -> usize {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1);
        self.queue.push_back(Waiter { key, waker });
        key
    }

    fn update(&mut self, key: usize, waker: Waker) -> usize {
        if self.is_in_waiting_range(key) {
            if let Some(w) = self.queue.iter_mut().find(|w| w.key == key) {
                w.waker = waker;
                return key;
            }
        }
        self.notified_count -= 1; // the waiter was already notified, so we need to decrement the number actively notified tasks
        self.insert(waker)
    }

    #[cold]
    fn remove(&mut self, key: usize) -> bool {
        if self.is_in_waiting_range(key) {
            if let Some(idx) = self.queue.iter().position(|w| w.key == key) {
                self.queue.remove(idx);
                return false;
            }
        }
        self.notified_count -= 1;
        true
    }

    fn cancel(&mut self, key: usize) -> bool {
        if self.remove(key) {
            self.notify_first()
        } else {
            false
        }
    }

    fn remove_if_notified(&mut self, key: usize, cx: &Context<'_>) -> bool {
        // all we really need to do here is decrement notified_count if the key isn't in the queue
        if self.is_in_waiting_range(key) {
            if let Some(w) = self.queue.iter_mut().find(|w| w.key == key) {
                w.waker = cx.waker().clone();
                return false;
            }
        }
        self.notified_count -= 1;
        true
    }

    fn notify_first(&mut self) -> bool {
        if let Some(waiter) = self.queue.pop_front() {
            self.notified_count += 1;
            debug_assert!(waiter.key >= self.min_key);
            self.min_key = waiter.key.wrapping_add(1);
            waiter.waker.wake();
            true
        } else {
            false
        }
    }

    fn notify_all(&mut self) -> bool {
        let num_notified = self.queue.len();
        while let Some(w) = self.queue.pop_front() {
            w.waker.wake();
        }
        self.notified_count += num_notified;
        self.min_key = self.next_key;
        num_notified > 0
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

        if !self.queue.is_empty() {
            flags |= WAITING;
        }

        if self.notified_count > 0 {
            flags |= NOTIFIED;
        }

        // Synchronise with `lock()`.
        self.waitlist.flags.store(flags, Ordering::SeqCst);
    }
}
