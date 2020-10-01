use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};

use futures::task::AtomicWaker;

pub struct Oneshot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    waker: AtomicWaker,
    // Setting has_value to true hands over control from the sender to the receiver. Setting it to
    // false hands over control from the receiver to the sender.
    has_value: AtomicBool,
}

pub struct Sender<'a, T>(&'a Oneshot<T>);
pub struct Receiver<'a, T>(&'a Oneshot<T>);

/// Asynchronously transfer a single value from one thread to another.
impl<T> Oneshot<T> {
    pub const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            waker: AtomicWaker::new(),
            has_value: AtomicBool::new(false),
        }
    }

    /// NOTE(unsafe): This function must be used at most once until the value is taken again.
    pub unsafe fn put(&self, value: T) {
        self.value.get().write(MaybeUninit::new(value));
        self.has_value.store(true, Ordering::Release);
        self.waker.wake();
    }

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

    pub unsafe fn recv<'a>(&'a self) -> Receiver<'a, T> {
        Receiver(self)
    }

    pub fn is_empty(&self) -> bool {
        !self.has_value.load(Ordering::Acquire)
    }

    /// Split the Oneshot into a Sender and a Receiver. The Sender can send one message. The
    /// Receiver is a Future and can be used to await that message. If the Receiver is dropped
    /// before taking the message, the message is dropped as well. This function can be called
    /// again after the lifetimes of the Sender and Receiver end in order to send a new message.
    pub fn split<'a>(&'a mut self) -> (Sender<'a, T>, Receiver<'a, T>) {
        unsafe { self.take() };
        (Sender(self), Receiver(self))
    }
}

impl<T> Drop for Oneshot<T> {
    fn drop(&mut self) {
        unsafe { self.take() };
    }
}

impl<'a, T> Sender<'a, T> {
    pub fn send(self, value: T) {
        unsafe { self.0.put(value) };
    }
}

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

impl<'a, T> Drop for Receiver<'a, T> {
    fn drop(&mut self) {
        unsafe { self.0.take() };
    }
}
