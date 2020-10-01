use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::{AcqRel, Acquire, Release};
use core::task::{Context, Poll, Waker};

const CAPACITY: usize = 16;

// Since fetch_max is unstable we use the poor man's fetch_or instead.

// The slot is not ready and does not contain a waker.
const DOZING: usize = 0;
// The slot is not ready and contains a waker.
const ASLEEP: usize = 1;
// The slot is ready and does not contain a waker.
const WAKING: usize = 3;
// The future is dropped and the slot does not contain a waker.
const DEAD: usize = 7;

struct Slot {
    state: AtomicUsize,
    waker: UnsafeCell<MaybeUninit<Waker>>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            state: AtomicUsize::new(DOZING),
            waker: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if *self.state.get_mut() == ASLEEP {
            drop(unsafe { self.waker.get().read().assume_init() });
        }
    }
}

impl Slot {
    // Return true if the slot is woken, sets the waker otherwise. Must not run concurrently with
    // itself but can run concurrently with wake.
    // This method is called from a unique reference of the Lock that owns it so it can't run
    // concurrently with itself. It also can't be in the DEAD state during this method.
    // When this method returns true, it will never be called again so it resets the slot
    // beforehand.
    #[inline]
    fn ready(&self, waker: &Waker) -> bool {
        let state = self.state.compare_and_swap(ASLEEP, DOZING, Acquire);
        if state == ASLEEP {
            // If we were ASLEEP, we now have an old waker we need to drop.
            drop(unsafe { self.waker.get().read().assume_init() });
        } else if state == WAKING {
            return true;
        }
        let waker = MaybeUninit::new(waker.clone());
        unsafe { self.waker.get().copy_from(&waker, 1) };
        let state = self.state.compare_and_swap(DOZING, ASLEEP, Release);
        if state != ASLEEP {
            drop(unsafe { waker.assume_init() });
        }
        state == WAKING
    }

    // Wake a slot. Return true if the slot is woken, false if it is dead. Can run concurrently
    // with itself, ready and kill.
    fn wake(&self) -> bool {
        let state = self.state.fetch_or(WAKING, Acquire);
        if state == ASLEEP {
            unsafe { self.waker.get().read().assume_init() }.wake();
        }
        state != DEAD
    }

    // Kill a slot.
    fn kill(&self) {
        if self.state.swap(DEAD, Acquire) == ASLEEP {
            drop(unsafe { self.waker.get().read().assume_init() });
        }
    }

    // Reset a slot to DOZING before returning it to the pool. This cannot happen after it is newly
    // allocated since it might be woken at any time. It also cannot happen while it is unallocated
    // since multiple allocators might be contesting for it. It must therefor happen before being
    // deallocated.
    fn reset(&self) {
        if self.state.swap(DOZING, Acquire) == ASLEEP {
            drop(unsafe { self.waker.get().read().assume_init() });
        }
    }
}

// front <= back
pub struct Mutex<T> {
    value: UnsafeCell<T>,
    // The front of the queue contains the waker currently being woken.
    front: AtomicUsize,
    // The back of the queue is where new wakers are added.
    back: AtomicUsize,
    slots: [Slot; CAPACITY],
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            value: UnsafeCell::new(value),
            front: AtomicUsize::new(0),
            back: AtomicUsize::new(0),
            slots: Default::default(),
        }
    }

    // This is the only method that can change `back`.
    fn register(&self) -> Option<&Slot> {
        let mut back = self.back.load(Acquire);
        loop {
            let used = back - self.front.load(Acquire);
            if used >= self.slots.len() {
                return None;
            }
            match self
                .back
                .compare_exchange_weak(back, back + 1, AcqRel, AcqRel)
            {
                Err(next_back) => back = next_back,
                Ok(_) => {
                    let slot = self.get(back);
                    let used = back - self.front.load(Acquire);
                    if used == 0 {
                        slot.wake();
                    }
                    return Some(slot);
                }
            }
        }
    }

    // End the access of the current slot. This is the only method that can change `front`. It
    // doesn't run concurrently with itself because it is only called from the drop of the guard.
    fn next(&self) {
        loop {
            // First, acquire self.get(front) and reset it.
            let front = self.front.load(Acquire);
            self.get(front).reset();
            // Then release it and synchronize access to front and back.
            let front = self.front.fetch_add(1, AcqRel).wrapping_add(1);
            let back = self.back.load(Acquire);
            if back == front || self.get(front).wake() {
                return;
            }
            // Woke a dead slot, try next.
        }
    }

    #[inline]
    fn get(&self, i: usize) -> &Slot {
        assert!(self.slots.len().is_power_of_two());
        &self.slots[i & (self.slots.len() - 1)]
    }

    pub fn lock(&self) -> Lock<'_, T> {
        Lock {
            mutex: self,
            slot: None,
        }
    }
}

pub struct Guard<'a, T>(&'a Mutex<T>);

impl<'a, T> Deref for Guard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.0.value.get() }
    }
}

impl<'a, T> DerefMut for Guard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.value.get() }
    }
}

impl<'a, T> Drop for Guard<'a, T> {
    fn drop(&mut self) {
        self.0.next();
    }
}

pub struct Lock<'a, T> {
    mutex: &'a Mutex<T>,
    slot: Option<&'a Slot>,
}

impl<'a, T> Drop for Lock<'a, T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            slot.kill();
        }
    }
}

impl<'a, T> Future for Lock<'a, T> {
    type Output = Guard<'a, T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Rust Futures should not do anything until awaited. Only once a Future is being awaited
        // is it safe to try to acquire the lock. If we allocate a slot earlier and then delay
        // awaiting the lock for a while, it is possible the lock's turn comes and isn't used until
        // much later, which means every other operation waiting for the lock is blocked.

        // Rust async functions work differently from most languages: In most languages an async
        // function will immediately begin making progress. For example, an async http request in
        // javascript would immediately send its request so that by the time the result is awaited,
        // there's a good chance it's ready immediately.
        let slot = self
            .slot
            .or_else(|| self.mutex.register())
            .expect("mutex slots exhausted");

        if slot.ready(cx.waker()) {
            self.slot = None;
            Poll::Ready(Guard(self.mutex))
        } else {
            self.slot = Some(slot);
            Poll::Pending
        }
    }
}
