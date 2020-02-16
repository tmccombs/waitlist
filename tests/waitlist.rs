mod mock_waker;

use mock_waker::MockWaker;
use waitlist::*;

fn wait_for_waker<'a>(wl: &'a Waitlist, w: &MockWaker) -> WaitHandle<'a> {
    let mut handle = wl.wait();
    handle.set_context(&w.to_context());
    handle
}

fn add_all<'a>(wl: &'a Waitlist, wakers: &[MockWaker]) -> Vec<WaitHandle<'a>> {
    wakers.iter().map(|w| wait_for_waker(wl, w)).collect()
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
    let mut k1 = wait_for_waker(&waitlist, &w1);
    let w2 = MockWaker::new();
    let _k2 = wait_for_waker(&waitlist, &w2);

    assert!(waitlist.notify_any());
    assert!(!waitlist.notify_any());
    assert_eq!(1, w1.notified_count());
    assert_eq!(0, w2.notified_count());
    assert!(k1.finish());
    assert!(waitlist.notify_any());
    assert_eq!(1, w2.notified_count());
}

#[test]
fn cancel_notifies_next() {
    let w1 = MockWaker::new();
    let w2 = MockWaker::new();
    let waitlist = Waitlist::new();

    let mut k1 = wait_for_waker(&waitlist, &w1);
    let _k2 = wait_for_waker(&waitlist, &w2);

    waitlist.notify_one();
    assert!(k1.cancel());
    assert_eq!(1, w2.notified_count(), "Second task wasn't notified");
}

#[test]
fn try_finish_works() {
    let waitlist = Waitlist::new();
    let waker = MockWaker::new();
    let mut cx = waker.to_context();
    let mut waiter = waitlist.wait();

    waiter.set_context(&mut cx); // start waiting

    assert!(!waiter.try_finish(&mut cx));
    assert!(!waiter.try_finish(&mut cx));
    waitlist.notify_one();
    assert!(waiter.try_finish(&mut cx));

    // after finishing, it should continue to return true
    assert!(waiter.try_finish(&mut cx));
}

#[test]
fn notify_after_clearing() {
    let waitlist = Waitlist::new();
    let w1 = MockWaker::new();
    let k1 = wait_for_waker(&waitlist, &w1);
    waitlist.notify_one();
    assert_eq!(1, w1.notified_count());

    let _k2 = wait_for_waker(&waitlist, &w1);
    let w2 = MockWaker::new();
    let k3 = wait_for_waker(&waitlist, &w2);
    waitlist.notify_all();

    assert_eq!(2, w1.notified_count());
    assert_eq!(1, w2.notified_count());
    drop(k1);
    drop(k3);
    let _k2 = wait_for_waker(&waitlist, &w2);
    assert!(waitlist.notify_one());
    assert_eq!(2, w2.notified_count());
}

#[test]
fn update() {
    let waitlist = Waitlist::new();

    let w1 = MockWaker::new();
    let mut k1 = wait_for_waker(&waitlist, &w1);
    let w2 = MockWaker::new();
    k1.set_context(&w2.to_context());
    waitlist.notify_all();
    assert_eq!(0, w1.notified_count());
    assert_eq!(1, w2.notified_count());
    k1.set_context(&w1.to_context());
    waitlist.notify_all();
    assert_eq!(1, w1.notified_count());

    let _k2 = wait_for_waker(&waitlist, &MockWaker::new());
    let _k3 = wait_for_waker(&waitlist, &MockWaker::new());
    k1.set_context(&w2.to_context());
    waitlist.notify_all();
    assert_eq!(2, w2.notified_count());
}
