//! Polling shell for one owned view-fold continuation.

use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use aether_bloomery_kinds::ReadArtifactResult;
use aether_bloomery_view::{PendingArtifact, ResolveError, ResolverDriver};

pub enum ResolutionPoll<T> {
    Finished(T),
    NeedArtifact(PendingArtifact),
    Stalled(Option<ResolveError>),
}

pub struct Resolution<T> {
    driver: ResolverDriver,
    future: Pin<Box<dyn Future<Output = T> + Send + 'static>>,
}

impl<T> Resolution<T> {
    pub fn new(driver: ResolverDriver, future: Pin<Box<dyn Future<Output = T> + Send + 'static>>) -> Self {
        Self { driver, future }
    }

    pub fn fulfill(&self, result: ReadArtifactResult) {
        self.driver.fulfill(result);
    }

    pub fn poll(&mut self) -> ResolutionPoll<T> {
        match self.future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(finished) => ResolutionPoll::Finished(finished),
            Poll::Pending => match self.driver.take_pending() {
                Some(pending) => ResolutionPoll::NeedArtifact(pending),
                None => ResolutionPoll::Stalled(self.driver.terminal()),
            },
        }
    }
}

impl<T> Drop for Resolution<T> {
    fn drop(&mut self) {
        self.driver.cancel();
    }
}
