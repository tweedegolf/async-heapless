// # Build your own async primitive

// Concurrency isn't easy and implementing its primitives is even harder. I found myself in need of
// some no-std, no-alloc Rust async concurrency primitives and decided to write some. I kept the
// scope small so even you and I can understand it. Even so, it still involved futures, wakers,
// atomics, drop and unsafe. I'll introduce each of those to you while building a simple primitive.
// At the end, even you will be able to implement your own primitives!

// First, our primitive:

//! A oneshot channel is a synchronization primitive used to send a single
//! message from one task to another. When the sender has sent a message and it
//! is ready to be received, the oneshot notifies the receiver. The receiver is
//! a Rust async `Future` that can be `await`ed. Our implementation does not
//! depend on std or alloc.
//!
//! See https://tweedegolf.nl/blog/50/async-oneshot for a full description of
//! the internals.

// What do we mean by all that? First, not depending on std or alloc means we can't use many
// common libraries. Rust async is still a new area and the libraries so far depend on
// allocating memory at least. That makes their primitives easier to build but also unusable on
// embedded devices which can't afford an allocator. I work with such devices regularly and so I've
// learned to avoid allocation whenever possible. We'll see that we can write our oneshot without
// allocation just fine.

// Next, we don't want to block the entire CPU while waiting for the message to be sent. For one
// thing, it won't be sent if the CPU is blocked. When an operating system is available, it can
// schedule the sending task while the receiving task is blocked and there's no problem. Small
// embedded devices usually can't afford an operating system though, so you're left with
// implementing task switching manually. That can involve a lot of moving state from your only call
// stack into a struct and back so you can use the call stack to continue another task.

// Rust async can do it for you: First, you choose an async executor to run your tasks. These
// executors can be much simpler than even an embedded OS. Then you write your tasks as normal.
// When you reach a point where you need to wait for another task to finish something, you just
// write `.await`. The executor will store the state of your call stack and continue with a task
// that isn't blocked. When the thing you were waiting for is done, the executor will wake your
// task up again.

// Let's begin with a `Oneshot` struct which will contain the data both the sender and receiver
// need to access in order to transfer the message. It needs to be created high on the call stack
// so it will outlive all tasks that might reference it. Then we create a `Sender` and `Receiver`
// struct which contain a reference to the `Oneshot`. We can then give the sender to one task and
// the receiver to another. Since they both have a reference to the `Oneshot`, they must be shared
// references, which means we can't safely modify just anything inside. Instead, we need to use
// types which are safe to modify through shared references.

// ### The message

// First, we need a way to transfer the message from the sender to the receiver. We could move it
// directly if we wait for both the sender and receiver to be ready for the transfer: We'd know
// where the data is on the sender side and where it should go on the receiver side. We call this
// synchronous communication, both the sender and the receiver need to be ready before the
// communication happens. It has its uses, but in this case, we don't want the sender to wait for
// the receiver since it won't get a response back anyway. Instead, we'll store the message in the
// `Oneshot` where it can wait for the receiver to be ready.

// We don't want to restrict our oneshot to just one kind of message, so we'll generalize it over
// the type of the message. We'll call that type `T`. When we create a new `Oneshot` we won't have
// a message for it yet so we need to tell Rust we'll initialize it later. In Rust, you'd usually
// use an `Option<T>` but we're going to track whether we have a `T` in a separate field so we'll
// use a `MaybeUninit<T>` instead. Finally, we need to modify the field through a shared reference.

// We'll store an `UnsafeCell<MaybeUninit<T>>` which allows us to modify the message through a
// shared reference as long as we do so in a block of code marked as `unsafe`. An `unsafe` block
// just means we make it our own responsibility to check the code is safe instead of the
// compiler's. We need to do this whenever the safety logic is too complicated for the compiler to
// understand.

// ### The synchronization

// Next, we need to remember whether the message is ready to be received, or in other words,
// whether the `message` field has been initialized. This is not as easy as it seems. Both
// compilers and processors will reorder memory accesses in order to make your code run faster.
// They'll make sure the code in a single task seems like it ran in the right order, but they won't
// give the same guarantee for code running in different tasks. For example, if we had used an
// `Option<T>` to store the message, the sender might turn it into a `Some()` first and store the
// message afterwards. If the receiver happened to check in between, it would try to read a message
// before there was ever stored one and it would end up with a bunch of garbage.

// In order to make concurrent memory accesses safe, atomics were invented. We'll remember if we
// have a message with an `AtomicBool`. Atomics make sure no task sees a partial update of the
// atomic. The atomic either updates completely or not at all. On top of that, it also helps us
// synchronize access to other memory: The value of the `AtomicBool` determines whether we allow
// the sender or the receiver to access the message. That way, they never access it at the same
// time.

// Finally, we want to use Rust async/await to wait until the message is ready. This requires us to
// store a waker. To be able to update the waker from a shared reference, we use the very nice
// `AtomicWaker` from the `futures` library.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use futures::task::AtomicWaker;

/// Transfer a single message between tasks using async/await.
pub struct Oneshot<T> {
    message: UnsafeCell<MaybeUninit<T>>,
    waker: AtomicWaker,
    has_message: AtomicBool,
}

// The `Sender` and `Receiver` are just thin wrappers around `Oneshot` references. They are generic
// over both the type of the underlying message (`T`) and the lifetime of the reference (`'a`).
// Wrapping the references in a struct allows us to name them and dictate what operations they
// support.

pub struct Sender<'a, T>(&'a Oneshot<T>);
pub struct Receiver<'a, T>(&'a Oneshot<T>);

// It may seem like we're not going very fast but choosing the right data representation is often
// the hardest part of programming. From a good data representation, the implementation will follow
// naturally. We begin with the `new` function which allows the creation of a new `Oneshot`. We
// have no message yet so we don't initialize it and set `has_message` to false.

impl<T> Oneshot<T> {
    pub const fn new() -> Self {
        Self {
            message: UnsafeCell::new(MaybeUninit::uninit()),
            waker: AtomicWaker::new(),
            has_message: AtomicBool::new(false),
        }
    }

    // ### The unsafe

    // Next, we implement the most basic operations, putting a message in and taking it out again.
    // We'll get to synchronization later, so these will be unsafe functions. For performance
    // reasons, users may want to use these functions directly so we'll make them public. This is
    // fine as long as we document what properties must hold to use the function safely.

    // The `put` function will store a message. Since we're implementing a oneshot, we only expect
    // one communication so we assume there was no message before `put` gets called. This is a
    // property a user of this unsafe function must be told about!

    // As soon as we set `has_message` to true, the receiver may try to read the message so we must
    // make sure to write the message first. We also need to prevent the write of the message and
    // of `has_message` from being reordered. We do that by storing `has_message` with
    // `Ordering::Release`. This is a signal to the compiler that it must make sure any memory
    // accesses before it must be finished and visible to other cores.

    // Finally, we use the waker in order to schedule the receiving task to continue.

    /// NOTE(unsafe): This function must not be used when the oneshot might
    /// contain a message or a `Sender` exists referencing this oneshot. This
    /// means it can't be used concurrently with itself or the latter to run
    /// will violate that constraint.
    pub unsafe fn put(&self, message: T) {
        self.message.get().write(MaybeUninit::new(message));
        self.has_message.store(true, Ordering::Release);
        self.waker.wake();
    }

    // The `take` function will check if a message is available yet and take it out of the oneshot
    // if it is. This time, we use `Ordering::Acquire` in order to ensure all memory accesses after
    // loading `has_message` really happen **after** it. For every synchronization between tasks, a
    // `Release`-`Acquire` pair is needed. Sometimes synchronization needs to go both ways and
    // `Ordering::AcqRel` can be used to get both effects.

    // When we're done taking the message, we `Release` its memory again by setting `has_message`
    // to `false`. If two instances of `take` run concurrently, they might both reach the message
    // read before setting `has_message` to `false` and thus duplicate the message so we need to
    // disallow that in the safety contract.

    // On the other hand, if it is run concurrently with `put`, according to the contract of `put`,
    // there must be no message beforehand. If `take` loads `has_message` first, it finds no
    // message and returns. If `put` writes `has_message` first, there is guaranteed to be a
    // message ready so `take` takes it without issue. Therefor, a single `take` can be safely run
    // concurrently with a single `put`. This is exactly what we need for a oneshot channel. We
    // never expect either function to run more than once but a single message can be transferred
    // between tasks safely. Now to enforce that safety in the Rust type system.

    /// NOTE(unsafe): This function must not be used concurrently with itself,
    /// but it can be used concurrently with put.
    pub unsafe fn take(&self) -> Option<T> {
        if self.has_message.load(Ordering::Acquire) {
            let message = self.message.get().read().assume_init();
            self.has_message.store(false, Ordering::Release);
            Some(message)
        } else {
            None
        }
    }

    // ### The split

    // The `split` function splits the oneshot into a sender and a receiver. The sender can be used
    // to send one message and the receiver to receive one. The split function takes a unique
    // reference to the oneshot and returns two shared references to it with the same lifetime
    // `'a`. This means Rust will consider the oneshot to be uniquely borrowed until it is sure
    // both the sender's and receiver's lifetime have ended. It prevents us from making more than
    // one pair!

    // However, once the lifetimes end, the oneshot will no longer be borrowed, so nothing prevents
    // someone from calling `split` again. Is that a problem? Not really: It means the `Oneshot`
    // can be reused to send another single message. We still call it a oneshot because the sender
    // and receiver it creates can only be used once.

    // There is one thing we need to take care of however: The receiving task might stop caring and
    // drop its `Receiver` before the sender sends its message. In that case, the oneshot will
    // still contain a message after the lifetime of the references end. In order to allow the
    // `Oneshot` to be reused again, we need to remove that message.

    // Note that simply setting `has_message` to `false` is a problem because the message is stored
    // in a `MaybeUninit` which doesn't know by itself whether it has a message and so it also
    // doesn't know when it should `drop` the message. Values that are forgotten without being
    // dropped can leak resources or even cause undefined behavior! We always need to make sure the
    // message is initialized before setting `has_message` to `true` and dropped or moved elsewhere
    // before setting it to `false`.

    // The safe thing to do is to `take` any message out of the `MaybeUninit` into a type that
    // drops implicitly again. However, `take` is unsafe so before we use it, we must check its
    // contract: The contract states `take` may not be used concurrently with itself. In this case,
    // we can be sure of that because `split` owns the unique reference to the `Oneshot` and so
    // nobody else could have a reference through which they could call `take`.

    /// Split the Oneshot into a Sender and a Receiver. The Sender can send one
    /// message. The Receiver is a Future and can be used to await that message.
    /// If the Receiver is dropped before taking the message, the message is
    /// dropped as well. This function can be called again after the lifetimes
    /// of the Sender and Receiver end in order to send a new message.
    pub fn split<'a>(&'a mut self) -> (Sender<'a, T>, Receiver<'a, T>) {
        unsafe { self.take() };
        (Sender(self), Receiver(self))
    }

    // Even if a user is going to track when it's safe to send and receive manually, they might
    // still want to make use of the `Sender` and/or the `Receiver`. We can allow creating them
    // separately with functions marked unsafe. This allows a user to create a `Receiver` while
    // using `put` directly and preventing the overhead of a `Sender` for example.

    /// NOTE(unsafe): There must be no more than one `Receiver` at a time.
    /// `take` should not be called while a `Receiver` exists.
    pub unsafe fn recv<'a>(&'a self) -> Receiver<'a, T> {
        Receiver(self)
    }

    /// NOTE(unsafe): There must be no more than one `Sender` at a time. `put`
    /// should not be called while a `Sender` exists. The `Oneshot` must be
    /// empty when the `Sender` is created.
    pub unsafe fn send<'a>(&'a self) -> Sender<'a, T> {
        Sender(self)
    }

    // We define a simple `is_empty` function to check if a message was sent yet. This would
    // ordinarily not be useful since the send would know whether it has sent anything and the
    // point of the receiver is that it finds out when it tries to receive. It is still useful for
    // debugging and assertions however. Since we don't know what is being checked exactly and the
    // performance of debugging is not an issue, we should use the `AcqRel` ordering here.

    pub fn is_empty(&self) -> bool {
        !self.has_message.load(Ordering::AcqRel)
    }
}

// If we drop the `Oneshot`, we need to drop any message it might have as well.

impl<T> Drop for Oneshot<T> {
    fn drop(&mut self) {
        unsafe { self.take() };
    }
}

// The `Sender` is extremely simple: It allows you to call the `put` function, but only once. This
// is enforced by the fact that this `send` function doesn't take a reference to `self`, but
// instead consumes it. After using `send`, the `Sender`'s lifetime has ended and it can't be used
// again. Calling `put` once is safe because when creating the `Sender` using `split`, we ensured
// there was no message stored.

impl<'a, T> Sender<'a, T> {
    pub fn send(self, message: T) {
        unsafe { self.0.put(message) };
    }
}

// ### The future

// The receiver we get from `split` is a future that can be awaited for the message being sent.
// This is the last tricky bit in our implementation. A `Future` is a thing with a `poll` function
// which will be called by the executor. In it, it receives a pinned reference to itself and a
// context containing a waker. In our case, we don't care that the reference is pinned. The `poll`
// can return one of two things: `Poll::Ready(message)` indicates the future is done and the
// `.await` will return with the `message`. `Poll::Pending` means the future is not done yet and
// Rust will handle control back to the executor that called `poll` so it can find another task to
// run.

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
// only in case the message is not ready yet.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

impl<'a, T> Future for Receiver<'a, T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.0.waker.register(cx.waker());
        if let Some(message) = unsafe { self.0.take() } {
            Poll::Ready(message)
        } else {
            Poll::Pending
        }
    }
}

// If we drop the receiver, any message in the oneshot will not be used anymore. It will already be
// dropped eventually when the oneshot itself is dropped or reused, but we might save resources by
// dropping it early, so we drop the message here if it exists. Note that it is safe to do so
// because we have the unique reference to the receiver and the sender will never call `take` so it
// won't run concurrently.

impl<'a, T> Drop for Receiver<'a, T> {
    fn drop(&mut self) {
        unsafe { self.0.take() };
    }
}

// ### That's it

// And that wraps up the implementation of our oneshot. Not too bad, right? Hopefully you have a
// good idea now about what goes into a concurrency primitive. The code for this blog is available
// at https://github.com/tweedegolf/async-heapless so please create an issue or pull request if
// there's something wrong with it. Also see
// https://github.com/tweedegolf/async-spi/blob/main/src/common.rs where I use (the unsafe methods
// of) the oneshot to build an async SPI driver.

// I want to encourage you to try writing your own abstractions. For starters, you could create a
// channel with a capacity of one message where the sender and receiver can be used multiple times
// and the sender can block until there is space in the channel. After that, you could look into
// channels that can store multiple messages or allow multiple concurrent senders or receivers. Or
// maybe you want to build a primitive where both tasks wait for each other and then exchange
// messages at the same time. Just make sure you don't try to add everything at once, each of those
// things is hard enough on its own.

// Finally, remember to document every unsafe function with the conditions needed to use it safely!
