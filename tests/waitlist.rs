use std::future::Future;
use std::pin::Pin;

mod mock_waker;

use mock_waker::MockWaker;
use waitlist::*;

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
fn notify_any() {
    let waitlist = Waitlist::new();
    let w1 = MockWaker::new();
    let k1 = waitlist.insert(&w1.to_context());
    let w2 = MockWaker::new();
    let _k2 = waitlist.insert(&w2.to_context());

    assert!(waitlist.notify_any());
    assert!(!waitlist.notify_any());
    assert_eq!(1, w1.notified_count());
    assert_eq!(0, w2.notified_count());
    assert!(k1.remove());
    assert!(waitlist.notify_any());
    assert_eq!(1, w2.notified_count());
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

#[test]
fn wait_waits_until_notified() {
    let waitlist = Waitlist::new();
    let waker = MockWaker::new();
    let mut cx = waker.to_context();
    let mut waiter = waitlist.wait();
    let mut wait_fut = Pin::new(&mut waiter);

    assert!(wait_fut.as_mut().poll(&mut cx).is_pending());
    assert!(wait_fut.as_mut().poll(&mut cx).is_pending());
    waitlist.notify_one();
    assert!(wait_fut.as_mut().poll(&mut cx).is_ready());

    // after finishing, we should get it in pending state again
    assert!(wait_fut.as_mut().poll(&mut cx).is_pending());
    waitlist.notify_one();
    assert!(wait_fut.as_mut().poll(&mut cx).is_ready());
}

#[test]
fn notify_after_clearing() {
    let waitlist = Waitlist::new();
    let w1 = MockWaker::new();
    let k1 = waitlist.insert(&w1.to_context());
    waitlist.notify_one();
    assert_eq!(1, w1.notified_count());

    let _k2 = waitlist.insert(&w1.to_context());
    let w2 = MockWaker::new();
    let k3 = waitlist.insert(&w2.to_context());
    waitlist.notify_all();

    assert_eq!(2, w1.notified_count());
    assert_eq!(1, w2.notified_count());
    drop(k1);
    drop(k3);
    let _k2 = waitlist.insert(&w2.to_context());
    assert!(waitlist.notify_one());
    assert_eq!(2, w2.notified_count());
}

#[test]
fn update() {
    let waitlist = Waitlist::new();

    let w1 = MockWaker::new();
    let mut k1 = waitlist.insert(&w1.to_context());
    let w2 = MockWaker::new();
    k1.update(&w2.to_context());
    waitlist.notify_all();
    assert_eq!(0, w1.notified_count());
    assert_eq!(1, w2.notified_count());
    k1.update(&w1.to_context());
    waitlist.notify_all();
    assert_eq!(1, w1.notified_count());

    let _k2 = waitlist.insert(&MockWaker::new().to_context());
    let _k3 = waitlist.insert(&MockWaker::new().to_context());
    k1.update(&w2.to_context());
    waitlist.notify_all();
    assert_eq!(2, w2.notified_count());
}
