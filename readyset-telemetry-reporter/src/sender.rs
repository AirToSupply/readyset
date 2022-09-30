use std::sync::Arc;

use tokio::sync::mpsc::Sender;
use tokio::sync::{oneshot, Mutex};

use crate::error::{SenderError as Error, SenderResult as Result};
use crate::telemetry::{TelemetryBuilder, TelemetryEvent, *};

/// A struct that can be used to report payloads containing arbitrary telemetry data to the ReadySet
/// telemetry ingress.
#[derive(Debug, Clone)]
pub struct TelemetrySender {
    tx: Option<Sender<(TelemetryEvent, Telemetry)>>,
    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    no_op: bool,
}

impl TelemetrySender {
    /// Construct a new [`TelemetryReporter`] with the given API key.
    pub fn new(tx: Sender<(TelemetryEvent, Telemetry)>, shutdown_tx: oneshot::Sender<()>) -> Self {
        Self {
            tx: Some(tx),
            shutdown_tx: Arc::new(Mutex::new(Some(shutdown_tx))),
            no_op: false,
        }
    }

    /// Create a new "no-op" telemetry reporter.
    pub fn new_no_op() -> Self {
        Self {
            tx: None,
            shutdown_tx: Arc::new(Mutex::new(None)),
            no_op: true,
        }
    }

    /// Send a telemetry payload to Segment. If the initial request fails for a non-permanent
    /// reason (eg, not a 4XX or IO error), this function will retry with an exponential
    /// backoff, timing out at [`TIMEOUT`].
    ///
    /// If this reporter was initialized with an API key equal to [`HARDCODED_API_KEY`], this
    /// function is a no-op.
    pub fn send_event_with_payload(&self, event: TelemetryEvent, payload: Telemetry) -> Result<()> {
        tracing::debug!("sending {event:?} with payload {payload:?}");
        if self.no_op {
            tracing::debug!("Ignoring ({event:?} {payload:?}) in no-op mode");
            return Ok(());
        }

        match self.tx.as_ref() {
            Some(tx) => tx
                .try_send((event, payload))
                .map_err(|e| Error::Sender(e.to_string())),
            None => Err(Error::Sender("sender missing tx".into())),
        }
    }

    pub fn send_event(&self, event: TelemetryEvent) -> Result<()> {
        self.send_event_with_payload(event, TelemetryBuilder::new().build())
    }

    /// Any event sent after shutdown() is sent will fail
    pub async fn shutdown(&self) {
        let tx = self.shutdown_tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(()).expect("failed to shut down");
        } else {
            tracing::warn!("Received shutdown signal but dont have a sender");
        }
    }
}
