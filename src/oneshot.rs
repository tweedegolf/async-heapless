//! A oneshot channel allowing sending of a single value with async, without depending on std or
//! alloc.
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::Ordering;
use core::task::{Context, Poll};

// A few months ago, I was tasked to create a Rust async SPI driver for embedded devices. Async
// support for Rust is a fairly new feature and support for it on embedded devices is even newer.
// We found many common abstractions targeting async required either the standard library or at
// least an allocator. In our embedded projects, we work without those, so building the SPI driver
// included building some async primitives.

// Ferrous Systems have also posted some async primitives in their post ["async/await on embedded
// Rust"](https://ferrous-systems.com/blog/async-on-embedded/). They include an executor for async
// tasks, an async mutex, channel and simple timer and an example async I2C driver targeting nRF52
// devices. We are gladly making use of some of these, especially the executor.

// Ferrous Systems were the first to make progress with this. In fact, it is in large part thanks
// to them that async is now available on embedded devices. In their post ["async/await on embedded
// Rust"](https://ferrous-systems.com/blog/async-on-embedded/), they showed a proof of concept
// async embedded application, including an executor for async tasks, an async mutex, channel and
// simple timer and an async I2C driver. These have been very useful to us.

// We were still missing some things though: We'd need at least an async SPI driver and support for
// multiple async delays based on a single timer. This is what we set out to build

// - ferrous stuff is too hard to understand
// - ferrous didn't make async spi
// - ferrous' async i2c is tightly bound to hardware

// TODO: intro; rust, async/await; no std; no alloc; asynchronous

// A oneshot channel is a synchronization primitive used to send a single value from one task to
// another. When the sender has sent a value and it is ready to be received, the oneshot should
// notify the receiver. Because this is an async primitive, that happens by the receiver awaiting
// and the sender waking the receiver.

// Because this is a heapless abstraction, we need to store all data on the stack. We begin with a
// `Oneshot` struct which will contain the data both the sender and receiver need to access in
// order to transfer the value. Then we create a `Sender` and `Receiver` struct which contain a
// reference to the `Oneshot`. We can then give the sender to one task and the receiver to another.
// Since they both have a reference to the `Oneshot`, they must be shared references, which means
// we can't safely modify just anything inside. Instead, we need to use types which are safe to
// modify through shared references.

// ### The value

// First, we need a way to transfer the value from the sender to the receiver. We could move it
// directly if we wait for both the sender and receiver to be ready for the transfer: We'd know
// where the data is on the sender side and where it should go on the receiver side. We call this
// synchronous communication, both the sender and the receiver need to be ready before the
// communication happens. It has its uses, but in this case we're trying to build an asynchronous
// oneshot, one which allows the sender to forget about the value and do other things long before
// the receiver is ready to receive it. So instead, we'll store the value in the `Oneshot` where it
// can wait for the receiver to be ready.

// We don't want to restrict our oneshot to just one kind of value, so we'll generalize it over the
// type of the value. We'll call that type `T`. When we create a new `Oneshot` we won't have a
// value for it yet so we need to tell Rust we'll initialize it later. In Rust, you'd usually use
// an `Option<T>` but we're going to track whether we have a `T` in a separate field so we'll use a
// `MaybeUninit<T>` instead. Finally, we need to modify the field through a shared reference.

// We'll store an `UnsafeCell<MaybeUninit<T>>` which allows us to modify the value through a shared
// reference as long as we do so in a block of code marked as `unsafe`. An `unsafe` block just
// means we make it our own responsibility to check the code is safe instead of the compiler's.
// We need to do this whenever the safety logic is too complicated for the compiler to understand.

// ### The synchronization

// Next, we need to remember whether the value is ready to be received, or in other words, whether
// the value has been initialized. This is not as easy as it seems. Both compilers and processors
// will reorder memory accesses in order to make your code run faster. They'll make sure the code
// in a single task seems like it ran in the right order, but they won't give the same guarantee
// for code running in different tasks. For example, if we had used an `Option<T>` to store the
// value, the sender might turn it into a `Some()` first and store the value afterwards. If the
// receiver happened to check in between, it would try to read a value before there was ever stored
// one and it would end up with a bunch of garbage.

// In order to make concurrent memory accesses safe, people invented atomics. We'll remember if we
// have a value with an `AtomicBool`. Atomics make sure no task sees a partial update of the
// atomic. The atomic either updates completely or not at all. On top of that, it also helps us
// synchronize access to other memory.

// Finally, we want to use Rust async/await to wait until the value is ready. This requires us to
// store a waker. A waker is a structure through which

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicBool;
use futures::task::AtomicWaker;

/// Transfer a single value between tasks using async/await.
pub struct Oneshot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    waker: AtomicWaker,
    has_value: AtomicBool,
}

// The `Sender` and `Receiver` are just thin wrappers around `Oneshot` references. They are generic
// over both the type of the underlying value (`T`) and the lifetime of the reference (`'a`).
// Wrapping the references in a struct allows us to name them and dictate what operations they
// support.

pub struct Sender<'a, T>(&'a Oneshot<T>);
pub struct Receiver<'a, T>(&'a Oneshot<T>);

// ### Implementation

// It may seem like we're not going very fast but choosing the right data representation is often
// the hardest part of programming. From a good data representation, the implementation will follow
// naturally. We begin with the `new` function which allows the creation of a new `Oneshot`. We
// have no value yet so we don't initialize it and set `has_value` to false.

impl<T> Oneshot<T> {
    pub const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            waker: AtomicWaker::new(),
            has_value: AtomicBool::new(false),
        }
    }

    // Next, we implement the most basic operations, putting a value in and taking it out again.
    // We'll get to synchronization later, so these will be unsafe functions. For performance
    // reasons, users may want to use these functions directly so we'll make them public. This is
    // fine as long as we document what properties must hold to use the function safely.

    // The `put` function will store a value. Since we're implementing a oneshot, we only expect
    // one communication so we assume there was no value before `put` gets called. This is a
    // property a user of this unsafe function must be told about!

    // As soon as we set `has_value` to true, the receiver may try to read the value so we must
    // make sure to write the value first. We also need to prevent the write of the value and of
    // `has_value` from being reordered. We do that by storing `has_value` with
    // `Ordering::Release`. This is a signal to the compiler that it must make sure any memory
    // accesses before it must be finished and visible to other cores.

    // Finally, we use the waker in order to schedule the receiving task to continue.

    /// NOTE(unsafe): This function must not be used when the oneshot might contain a value or a
    /// `Sender` exists referencing this oneshot. This means it can't be used concurrently with
    /// itself or the latter to run will violate that constraint.
    pub unsafe fn put(&self, value: T) {
        self.value.get().write(MaybeUninit::new(value));
        self.has_value.store(true, Ordering::Release);
        self.waker.wake();
    }

    // The `take` function will check if a value is available yet and take it out of the oneshot if
    // it is. This time, we use `Ordering::Acquire` in order to ensure all memory accesses after
    // loading `has_value` really happen **after** it. For every synchronization between tasks, a
    // `Release`-`Acquire` pair is needed. Sometimes synchronization needs to go both ways and
    // `Ordering::AcqRel` can be used to get both effects.

    // When we're done taking the value, we `Release` its memory again by setting `has_value` to
    // `false`. If two instances of `take` run concurrently, they might both reach the value read
    // before setting `has_value` to `false` and thus duplicate the value so we need to disallow
    // that in the safety contract.

    // On the other hand, if it is run concurrently with `put`, according to the contract of `put`,
    // there must be no value beforehand. If `take` loads `has_value` first, it finds no value and
    // returns. If `put` writes `has_value` first, there is guaranteed to be a value ready so
    // `take` takes it without issue. Therefor, a single `take` can be safely run concurrently with
    // a single `put`. This is exactly what we need for a oneshot channel. We never expect either
    // function to run more than once but a single value can be transferred between tasks safely.
    // Now to enforce that safety in the Rust type system.

    /// NOTE(unsafe): This function must not be used concurrently with itself, but it can be used
    /// concurrently with put.
    pub unsafe fn take(&self) -> Option<T> {
        if self.has_value.load(Ordering::Acquire) {
            let value = self.value.get().read().assume_init();
            self.has_value.store(false, Ordering::Release);
            Some(value)
        } else {
            None
        }
    }

    // The `split` function splits the oneshot into a sender and a receiver. The sender can be used
    // to send one message and the receiver to receive one. The split function takes a unique
    // reference to the oneshot and returns two shared references to it with the same lifetime
    // `'a`. This means Rust will consider the oneshot to be uniquely borrowed until it is sure
    // both the sender's and receiver's lifetime have ended. It prevents us from making more than
    // one pair!

    // However, once the lifetimes end, the oneshot will no longer be borrowed, so
    // nothing prevents someone from calling `split` again. Is that a problem? Not really: It means
    // the `Oneshot` can be reused to send another single value. We still call it a oneshot because
    // the sender and receiver it creates can only be used once.

    // There is one thing we need to take care of however: The receiving task might stop caring and
    // drop its `Receiver` before the sender sends its message. In that case, the oneshot will
    // still contain a value after the lifetime of the references end. In order to allow the
    // `Oneshot` to be reused again, we need to remove that value. Since we own the unique
    // reference to `Oneshot`, we can safely `take` it. Note that simply setting `has_value` to
    // `false` is bad because the value would never be dropped. After taking it and going unused it
    // will be implicitly dropped.

    // TODO: Something about using unsafe{} for the first time and checking all properties hold.

    /// Split the Oneshot into a Sender and a Receiver. The Sender can send one message. The
    /// Receiver is a Future and can be used to await that message. If the Receiver is dropped
    /// before taking the message, the message is dropped as well. This function can be called
    /// again after the lifetimes of the Sender and Receiver end in order to send a new message.
    pub fn split<'a>(&'a mut self) -> (Sender<'a, T>, Receiver<'a, T>) {
        unsafe { self.take() };
        (Sender(self), Receiver(self))
    }

    // Even if a user is going to track when it's safe to send and receive manually, they might
    // still want to make use of the `Sender` and/or the `Receiver`. We can allow creating them
    // separately with functions marked unsafe. This allows a user to create a `Receiver` while
    // using `put` directly and preventing the overhead of a `Sender` for example.

    /// NOTE(unsafe): There must be no more than one `Receiver` at a time. `take` should not be
    /// called while a `Receiver` exists.
    pub unsafe fn recv<'a>(&'a self) -> Receiver<'a, T> {
        Receiver(self)
    }

    /// NOTE(unsafe): There must be no more than one `Sender` at a time. `put` should not be called
    /// while a `Sender` exists. The `Oneshot` must be empty when the `Sender` is created.
    pub unsafe fn send<'a>(&'a self) -> Sender<'a, T> {
        Sender(self)
    }

    // We define a simple `is_empty` function to check if a value was sent yet. This would
    // ordinarily not be useful since the send would know whether it has sent anything and the
    // point of the receiver is that it finds out when it tries to receive. It is still useful for
    // debugging and assertions however. Since we don't know what is being checked exactly and the
    // performance of debugging is not an issue, we should use the `AcqRel` ordering here.

    pub fn is_empty(&self) -> bool {
        !self.has_value.load(Ordering::AcqRel)
    }
}

// If we drop the `Oneshot`, we need to drop any value it might have as well.

impl<T> Drop for Oneshot<T> {
    fn drop(&mut self) {
        unsafe { self.take() };
    }
}

// The `Sender` is extremely simple: It allows you to call the `put` function, but only once. This
// is enforced by the fact that this `send` function doesn't take a reference to `self`, but
// instead consumes it. After using `send`, the `Sender`'s lifetime has ended and it can't be used
// again. Calling `put` once is safe because when creating the `Sender` using `split`, we ensured
// there was no value stored.

impl<'a, T> Sender<'a, T> {
    pub fn send(self, value: T) {
        unsafe { self.0.put(value) };
    }
}

// The receiver we get from `split` is a future that can be awaited for the value being sent. This
// is the last tricky bit in our implementation. A `Future` is a thing with a `poll` function. In
// it, it receives a pinned reference to the `Future` and a context containing a waker. It can
// return one of two things: `Poll::Ready(value)` indicates the future is done and the `.await`
// will return with the `value`. `Poll::Pending` means the future is not done yet and Rust will
// handle control back to the executor that called `poll` so it can find another task to run.

// The `Future` will be polled for the first time when it is first awaited. After that, in
// principle it won't run again until we use the waker we received in the previous `poll` to wake
// it again. This means we need to store the waker somewhere where the sending task can use it to
// wake the receiving task again. This is of course in the waker field of the `Oneshot`. Thanks to
// using an atomic waker, we don't need to worry if it's safe to store the waker, we can do it at
// any time.

// In practice, some executors poll the `Future` even if it hasn't been woken so it is always
// important to check if we're really done. In this case, we can safely call `take` since we have
// the unique reference to the `Receiver` and the `Sender` will never call `take`. Note that the
// documentation of
// [`AtomicWaker`](https://docs.rs/futures/0.3.6/futures/task/struct.AtomicWaker.html) states that
// consumers should call `register` before checking the result of a computation so we can't call it
// only in case the value is not ready yet.

impl<'a, T> Future for Receiver<'a, T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.0.waker.register(cx.waker());
        if let Some(value) = unsafe { self.0.take() } {
            Poll::Ready(value)
        } else {
            Poll::Pending
        }
    }
}

// If we drop the receiver, any value in the oneshot will not be used anymore. It will already be
// dropped eventually when the oneshot itself is dropped or reused, but we might save resources by
// dropping it early, so we drop the value here if it exists. Note that it is safe to do so because
// we have the unique reference to the receiver and the sender will never call `take` so it won't
// run concurrently.

impl<'a, T> Drop for Receiver<'a, T> {
    fn drop(&mut self) {
        unsafe { self.0.take() };
    }
}

// TODO: outro, example usage async-spi
