use http_body::{Body, Frame};
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{sleep, Sleep};

pin_project! {
    /// A wrapper around an http body that delays the start of the stream
    /// until the specified duration has passed.
    pub struct DelayedBody<B> {
        #[pin]
        inner: B,
        #[pin]
        delay: Sleep,
        delay_finished: bool,
    }
}

impl<B> DelayedBody<B> {
    /// Creates a new DelayedBody that waits for `duration` before polling the inner body.
    pub fn new(inner: B, duration: Duration) -> Self {
        Self {
            inner,
            delay: sleep(duration),
            delay_finished: false,
        }
    }
}

impl<B: Body> Body for DelayedBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();

        // Check if the delay has completed
        if !*this.delay_finished {
            match this.delay.poll(cx) {
                Poll::Ready(_) => {
                    // Timeout expired, mark as finished and proceed to poll inner body
                    *this.delay_finished = true;
                }
                Poll::Pending => {
                    // Still waiting for the timeout
                    return Poll::Pending;
                }
            }
        }

        // Delay is over, delegate to the inner body
        this.inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        // Even if inner is empty, we act as if the stream exists until the delay is over
        self.delay_finished && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
