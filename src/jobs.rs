// Adapted verbatim from green-vita (MPL-2.0, https://github.com/Day-OS/green-vita),
// src/jobs.rs - generic helper for polling a background Tokio task without blocking the
// render loop. See THIRD_PARTY_NOTICES.md.
//!
//! Shared helpers for polling background Tokio tasks without blocking the UI loop.

use anyhow::Result;
use tokio::task::JoinHandle;

pub(crate) enum PollJob<T> {
    Pending(JoinHandle<Result<T>>),
    Done(Result<T>),
}

impl<T> PollJob<T> {
    pub(crate) fn is_pending(&self) -> bool {
        matches!(self, PollJob::Pending(_))
    }
}

pub(crate) async fn poll_job<T>(handle: JoinHandle<Result<T>>) -> PollJob<T> {
    if !handle.is_finished() {
        return PollJob::Pending(handle);
    }
    PollJob::Done(
        handle
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("task failed: {error}"))),
    )
}
