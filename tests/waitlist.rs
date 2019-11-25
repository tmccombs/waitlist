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
