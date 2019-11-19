use alloc::sync::Arc;
use core::mem;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, RawWaker, RawWakerVTable, Waker};

pub struct MockWaker {
    inner: Inner,
    waker: Waker,
}

type Inner = Arc<AtomicUsize>;

const VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_mock, wake_mock, wake_by_ref_mock, drop_mock);

impl MockWaker {
    pub fn new() -> Self {
        let inner = Arc::new(AtomicUsize::new(0));
        let waker = make_waker(&inner);
        MockWaker { inner, waker }
    }

    pub fn to_context(&self) -> Context<'_> {
        Context::from_waker(&self.waker)
    }

    pub fn notified_count(&self) -> usize {
        self.inner.load(Ordering::SeqCst)
    }
}

impl Default for MockWaker {
    fn default() -> Self {
        Self::new()
    }
}

fn make_waker(inner: &Inner) -> Waker {
    let data = Arc::into_raw(inner.clone()) as *const ();
    let raw = RawWaker::new(data, &VTABLE);
    unsafe { Waker::from_raw(raw) }
}

unsafe fn clone_mock(data: *const ()) -> RawWaker {
    let inner: Inner = get_inner(data);
    let new_inner = inner.clone();
    mem::forget(inner);
    RawWaker::new(Arc::into_raw(new_inner) as *const (), &VTABLE)
}

unsafe fn wake_mock(data: *const ()) {
    let inner = get_inner(data);
    inner.fetch_add(1, Ordering::SeqCst);
}

unsafe fn wake_by_ref_mock(data: *const ()) {
    let inner = get_inner(data);
    inner.fetch_add(1, Ordering::SeqCst);
    mem::forget(inner);
}

unsafe fn drop_mock(data: *const ()) {
    mem::drop(get_inner(data));
}

unsafe fn get_inner(data: *const ()) -> Inner {
    Arc::from_raw(data as *const _)
}
