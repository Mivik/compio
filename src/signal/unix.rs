//! Unix-specific types for signal handling.

use crate::{
    driver::{Driver, Poller},
    task::RUNTIME,
};
use once_cell::unsync::Lazy as LazyCell;
use slab::Slab;
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

thread_local! {
    #[allow(clippy::type_complexity)]
    static HANDLER: LazyCell<RefCell<HashMap<i32, Slab<Box<dyn FnOnce()>>>>> =
        LazyCell::new(|| RefCell::new(HashMap::new()));
}

unsafe extern "C" fn signal_handler(sig: i32) {
    HANDLER.with(|handler| {
        let mut handler = handler.borrow_mut();
        if let Some(handlers) = handler.get_mut(&sig) {
            if !handlers.is_empty() {
                let handlers = std::mem::replace(handlers, Slab::new());
                for (_, handler) in handlers {
                    handler();
                }
            }
        }
    });
}

unsafe fn init(sig: i32) {
    libc::signal(sig, signal_handler as *const () as usize);
}

unsafe fn uninit(sig: i32) {
    libc::signal(sig, libc::SIG_DFL);
}

fn register(sig: i32, f: impl FnOnce() + 'static) -> usize {
    unsafe { init(sig) };

    HANDLER.with(|handler| {
        handler
            .borrow_mut()
            .entry(sig)
            .or_default()
            .insert(Box::new(f))
    })
}

fn unregister(sig: i32, key: usize) {
    let need_uninit = HANDLER.with(|handler| {
        let mut handler = handler.borrow_mut();
        if let Some(handlers) = handler.get_mut(&sig) {
            if handlers.contains(key) {
                let _ = handlers.remove(key);
            }
            if !handlers.is_empty() {
                return false;
            }
        }
        true
    });
    if need_uninit {
        unsafe { uninit(sig) };
    }
}

/// Represents a listener to unix signal event.
#[derive(Debug)]
pub struct SignalEvent {
    sig: i32,
    user_data: usize,
    handler_key: usize,
}

impl SignalEvent {
    pub(crate) fn new(sig: i32) -> Self {
        let user_data = RUNTIME.with(|runtime| runtime.submit_dummy());
        let handler_key = RUNTIME.with(|runtime| {
            // Safety: the runtime is thread-local static.
            let driver = unsafe {
                (runtime.driver() as *const Driver)
                    .as_ref()
                    .unwrap_unchecked()
            };
            register(sig, move || driver.post(user_data, 0).unwrap())
        });
        Self {
            sig,
            user_data,
            handler_key,
        }
    }
}

impl Future for SignalEvent {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        RUNTIME
            .with(|runtime| runtime.poll_dummy(cx, self.user_data))
            .map(|res| {
                unregister(self.sig, self.handler_key);
                res.map(|_| ())
            })
    }
}

impl Drop for SignalEvent {
    fn drop(&mut self) {
        unregister(self.sig, self.handler_key);
    }
}

/// Creates a new listener which will receive notifications when the current
/// process receives the specified signal.
pub fn signal(sig: i32) -> SignalEvent {
    SignalEvent::new(sig)
}