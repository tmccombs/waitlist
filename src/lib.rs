use std::collections::vec_deque::VecDeque;
use std::fmt;
use std::future::Future;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

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

// Set when there is at least one notifiable waker
const WAITING: usize = 1 << 1;

// Set when we at least one task has been notified, but hasn't
// yet been removed
const NOTIFIED: usize = 1 << 2;

/**
 */
pub struct Waitlist {
    flags: AtomicUsize,
    inner: Mutex<Inner>,
}

pub struct WaitHandle<'a> {
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
            inner: Mutex::new(Inner {
                queue: VecDeque::with_capacity(cap),
                notified_count: 0,
                min_key: 0,
                next_key: 0,
            }),
        }
    }

    fn lock(&self) -> Guard<'_> {
        Guard {
            flags: &self.flags,
            inner: self.inner.lock().unwrap(),
        }
    }

    #[inline]
    pub fn wait(&self) -> WaitHandle<'_> {
        WaitHandle {
            waitlist: self,
            key: None,
        }
    }

    #[inline]
    pub fn notify_one(&self) -> bool {
        if self.flags.load(Ordering::Relaxed) & WAITING != 0 {
            self.lock().notify_first()
        } else {
            false
        }
    }

    #[inline]
    pub fn notify_all(&self) -> bool {
        if self.flags.load(Ordering::Relaxed) & WAITING != 0 {
            self.lock().notify_all()
        } else {
            false
        }
    }

    #[inline]
    pub fn notify_any(&self) -> bool {
        let flags = self.flags.load(Ordering::Relaxed);
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

impl WaitHandle<'_> {
    /// Mark this task as completed.
    ///
    /// If this handle still has a waker on the queue,
    /// remove that waker without triggering another notify
    /// and return true. Otherwise, return false.
    #[inline]
    pub fn finish(&mut self) -> bool {
        if let Some(key) = self.key.take() {
            self.waitlist.lock().remove(key)
        } else {
            false
        }
    }

    /// Mark that the task was cancelled.
    ///
    /// If this handle currently has a waker on the queue and there is
    /// at least one other task waiting on the queue, remove this waker from the queue,
    /// wake the next task, and return true. Otherwise return false.
    #[inline]
    pub fn cancel(&mut self) -> bool {
        if let Some(key) = self.key.take() {
            self.waitlist.lock().cancel(key)
        } else {
            false
        }
    }

    #[inline]
    pub fn set_context(&mut self, cx: &Context) {
        let key = if let Some(key) = self.key {
            self.waitlist.lock().update(key, cx)
        } else {
            self.waitlist.lock().insert(cx)
        };
        self.key = Some(key);
    }

    /// Return true if the WaitHandle has been polled at least once, and has not been
    /// completed (by calling either `finish` or `cancel`).
    pub fn is_pending(&self) -> bool {
        self.key.is_some()
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(key) = self.key {
            if self.waitlist.lock().remove_if_notified(key, cx) {
                self.key = None;
                return Poll::Ready(());
            }
        } else {
            self.key = Some(self.waitlist.lock().insert(cx));
        }
        Poll::Pending
    }

    pub fn into_key(self) -> Option<usize> {
        let key = self.key;
        mem::forget(self);
        key
    }

    // should this be unsafe?
    /// Create a `WaitHandle` for a `Waitlist` using a key that was previously acquired from
    /// `into_key`.
    ///
    /// For this to work as expected, `key` should be a key returned by a previous call to `into_key`
    /// on a `WaitHandle` that was created from the same `waitlist`. This takes ownership of the wait
    /// entry for this key.
    ///
    /// You should avoid using this if possible, but in some cases it is necessary to avoid
    /// self-reference.
    pub fn from_key(waitlist: &Waitlist, key: Option<usize>) -> WaitHandle<'_> {
        WaitHandle { waitlist, key }
    }
}

impl<'a> Future for WaitHandle<'a> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(key) = self.key {
            if self.waitlist.lock().remove_if_notified(key, cx) {
                self.key = None;
                return Poll::Ready(());
            }
        } else {
            self.key = Some(self.waitlist.lock().insert(cx));
        }
        Poll::Pending
    }
}

impl<'a> Drop for WaitHandle<'a> {
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

    fn insert(&mut self, cx: &Context<'_>) -> usize {
        let key = self.next_key;
        let waker = cx.waker().clone();
        self.next_key = self.next_key.wrapping_add(1);
        self.queue.push_back(Waiter { key, waker });
        key
    }

    fn update(&mut self, key: usize, cx: &Context<'_>) -> usize {
        if self.is_in_waiting_range(key) {
            if let Some(w) = self.queue.iter_mut().find(|w| w.key == key) {
                w.waker = cx.waker().clone();
                return key;
            }
        }
        self.notified_count -= 1; // the waiter was already notified, so we need to decrement the number of actively notified tasks
        self.insert(cx)
    }

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
    flags: &'a AtomicUsize,
    inner: MutexGuard<'a, Inner>,
}

impl<'a> Deref for Guard<'a> {
    type Target = Inner;

    #[inline]
    fn deref(&self) -> &Inner {
        &*self.inner
    }
}

impl<'a> DerefMut for Guard<'a> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Inner {
        &mut *self.inner
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

        // Update flags. Use relaxed ordering because
        // releasing the mutex will create a memory boundary.
        self.flags.store(flags, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures_task::noop_waker;

    #[test]
    fn wraparound() {
        const KEY_START: usize = usize::max_value() - 1;
        let mut inner = Inner {
            queue: VecDeque::new(),
            notified_count: 0,
            min_key: KEY_START,
            next_key: KEY_START,
        };

        let waker = noop_waker();
        let context = Context::from_waker(&waker);

        inner.insert(&context);
        let k2 = inner.insert(&context);
        let k3 = inner.insert(&context);
        assert_eq!(0, k3);
        assert_eq!(1, inner.next_key);
        assert!(inner.notify_first());
        assert_eq!(usize::max_value(), inner.min_key);
        assert!(inner.is_in_waiting_range(k2));
        assert!(inner.is_in_waiting_range(k3));
        assert_eq!(0, inner.update(0, &context));
        assert!(!inner.remove(0));
        assert!(!inner.remove(k2));
    }
}
