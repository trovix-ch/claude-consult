//! The progress heartbeat: one progress notification a second for the whole call.
//!
//! A panel runs for minutes, and clients abort a tool call that goes silent for their
//! idle window (five minutes on HTTP transport). So the line goes out on a clock,
//! changed or not: it is the heartbeat as well as the display. That gives up the idle
//! window as a guard against a hung provider request; what bounds one now is the
//! request's read timeout, and a response that keeps trickling is bounded only by the
//! client's total tool timeout.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use consult_core::display::ProgressStyle;
use consult_core::panel::{Progress, progress_line};
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::{Peer, RoleServer};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Seconds between progress lines.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Where progress lines go. Abstracted so the ticker can be tested without a client.
pub trait ProgressSink: Send + Sync + 'static {
    /// Sends one line: `message`, the `progress` value and an optional `total`.
    /// An error is reported to the ticker, which drops it.
    fn send(
        &self,
        message: String,
        progress: f64,
        total: Option<f64>,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

/// A sink that sends `notifications/progress` to the client that made the request.
#[derive(Clone, Debug)]
pub struct PeerSink {
    peer: Peer<RoleServer>,
    token: ProgressToken,
}

impl PeerSink {
    /// A sink for the request that carried `token`.
    pub fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        Self { peer, token }
    }
}

impl ProgressSink for PeerSink {
    async fn send(&self, message: String, progress: f64, total: Option<f64>) -> Result<(), String> {
        let mut param =
            ProgressNotificationParam::new(self.token.clone(), progress).with_message(message);
        if let Some(total) = total {
            param = param.with_total(total);
        }
        self.peer
            .notify_progress(param)
            .await
            .map_err(|e| e.to_string())
    }
}

/// A running ticker. [`Ticker::stop`] cancels it and waits for it to end; dropping it
/// cancels it without waiting, so a handler that is itself dropped leaves no ticker
/// behind.
#[derive(Debug)]
pub struct Ticker {
    stop: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Starts sending `progress`'s line to `sink` now and then every `interval`.
    pub fn start<S: ProgressSink>(
        sink: S,
        progress: Arc<Progress>,
        style: ProgressStyle,
        interval: Duration,
    ) -> Self {
        let stop = CancellationToken::new();
        let child = stop.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (message, value, total) = progress_line(&progress, style, None);
                // Progress must never break or slow a review: a failed send is dropped
                // and the next one tries again. A send still pending at cancel is
                // abandoned, so stopping never waits on a slow client.
                tokio::select! {
                    biased;
                    _ = child.cancelled() => break,
                    sent = sink.send(message, value, total) => {
                        if let Err(e) = sent {
                            tracing::debug!("progress notification dropped: {e}");
                        }
                    }
                }
                tokio::select! {
                    biased;
                    _ = child.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Cancels the ticker and waits for it, so no notification can follow the result.
    pub async fn stop(mut self) {
        self.stop.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Stops the ticker if there is one.
pub async fn stop(ticker: Option<Ticker>) {
    if let Some(t) = ticker {
        t.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One line as sent: (message, progress, total).
    type Line = (String, f64, Option<f64>);

    #[derive(Clone, Default)]
    struct Fake {
        sent: Arc<Mutex<Vec<Line>>>,
        attempts: Arc<AtomicUsize>,
        fail: bool,
    }

    impl ProgressSink for Fake {
        async fn send(
            &self,
            message: String,
            progress: f64,
            total: Option<f64>,
        ) -> Result<(), String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err("client went away".into());
            }
            self.sent
                .lock()
                .expect("lock")
                .push((message, progress, total));
            Ok(())
        }
    }

    #[tokio::test]
    async fn fires_on_the_clock_and_stops_after_cancel() {
        let sink = Fake::default();
        let progress = Arc::new(Progress::new("consult"));
        let ticker = Ticker::start(
            sink.clone(),
            progress,
            ProgressStyle::Count,
            Duration::from_millis(20),
        );
        tokio::time::sleep(Duration::from_millis(210)).await;
        ticker.stop().await;
        let n = sink.sent.lock().expect("lock").len();
        // Nothing changed during the call, and the line kept coming: it is the heartbeat.
        assert!(n >= 5, "only {n} lines in 210ms at 20ms");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            sink.sent.lock().expect("lock").len(),
            n,
            "progress kept coming after stop"
        );
        for (message, _, total) in sink.sent.lock().expect("lock").iter() {
            assert_eq!(message, "consult · 0/0 finished · 0 tool calls");
            assert_eq!(*total, None);
        }
    }

    #[tokio::test]
    async fn the_first_line_goes_out_at_once() {
        let sink = Fake::default();
        let ticker = Ticker::start(
            sink.clone(),
            Arc::new(Progress::new("cleanroom")),
            ProgressStyle::Full,
            Duration::from_secs(60),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        ticker.stop().await;
        let sent = sink.sent.lock().expect("lock").clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].0.starts_with("cleanroom · "), "{}", sent[0].0);
    }

    #[tokio::test]
    async fn a_failing_sink_keeps_ticking() {
        let sink = Fake {
            fail: true,
            ..Fake::default()
        };
        let ticker = Ticker::start(
            sink.clone(),
            Arc::new(Progress::new("consult")),
            ProgressStyle::Percent,
            Duration::from_millis(10),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        ticker.stop().await;
        assert!(sink.attempts.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn dropping_the_ticker_cancels_it() {
        let sink = Fake::default();
        let ticker = Ticker::start(
            sink.clone(),
            Arc::new(Progress::new("consult")),
            ProgressStyle::Full,
            Duration::from_millis(10),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(ticker);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let n = sink.sent.lock().expect("lock").len();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(sink.sent.lock().expect("lock").len(), n);
    }
}
