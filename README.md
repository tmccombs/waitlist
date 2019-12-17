[![Coverage Status](https://coveralls.io/repos/github/tmccombs/waitlist/badge.svg?branch=master)](https://coveralls.io/github/tmccombs/waitlist?branch=master)
[![Build Status](https://github.com/tmccombs/waitlist/workflows/Rust/badge.svg)](https://github.com/tmccombs/waitlist/actions?query=workflow%3ARust+branch%3Amaster)


# Waitlist

In Rust async programming it is somewhat common to need to keep track of a set of `Waker`s that should be notified when something happens. This is useful for implementing many synchronization abstractions including mutexes, channels, condition variables, etc. This library provides an implementation of a queue of `Waker`s, that can be used for this purpose.

## Acknowledgements

The implementation (and API) pulls heavily from the [`waker_set`](https://github.com/async-rs/async-std/blob/master/src/sync/waker_set.rs) module in [`async-std`](https://github.com/async-rs/async-std), and the storage structure was inspired by [`slab`](https://github.com/carllerche/slab), although the actual details differ somewhat to optimize for the uses of `WaitList`.

## Differences from `async-std` and `futures-util`

This implementation differs from the `waker_set` implementation and patterns followed in the `futures-util` crate. Specifically:
  1. The order in which tasks are notified is more fair. `Waitlist` uses a FIFO queue for notifying waiting tasks, whereas the usage of `slab` in other implementations can result in task starvation in certains situations (see https://users.rust-lang.org/t/concerns-about-using-slab-to-track-wakers/33653).
  2. Removing an entry from the list is potentially `O(n)` rather than `O(1)`. This is a bit of a tradeoff. Using a single linked list instead of a double linked list reduces complexity, as well as the amount of memory needed for each entry, but also means removal is more expensive. Using slab gets `O(1)` removal because it doesn't care about the order of the entries. On the other hand, notifying a single entry is `O(1)` with `Waitlist`, and notifying all waiting only has to iterate through waiting entries, whereas with slab it is necessary to iterate through the entire capacity of the slab.
  3. `WaitList` uses atomics to synchronize similar to `async-std` and unlike `futures-util` which uses a `Mutex`.
