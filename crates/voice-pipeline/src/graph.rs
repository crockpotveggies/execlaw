//! Two-lane Tokio graph: system lane preempts data lane (§2.13.2).
//!
//! Every pipeline stage consumes from BOTH lanes via `tokio::select!`
//! where the system lane is polled first, guaranteeing an `Interruption`
//! reaches every stage within a few ms.
//!
//! This module provides the plumbing (lane senders, a `PipelineHandle`
//! to fan-out system frames to all stages). Individual stages (VAD, STT
//! client, LLM orchestrator, TTS client) are built on top of it.

use crate::{DataFrame, SystemFrame};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, mpsc};

/// Data-lane channel capacity. Small — we want backpressure to reach
/// the producer quickly.
pub const DATA_LANE_CAPACITY: usize = 64;
/// System-lane channel capacity. Larger since lag here is catastrophic
/// (interrupts getting dropped).
pub const SYSTEM_LANE_CAPACITY: usize = 256;

/// A single pipeline instance — one call / one conversation.
pub struct Pipeline {
    system_tx: broadcast::Sender<SystemFrame>,
    data_tx: Arc<Mutex<Option<mpsc::Sender<DataFrame>>>>,
}

impl Pipeline {
    pub fn new() -> (Self, PipelineEnds) {
        let (system_tx, _) = broadcast::channel(SYSTEM_LANE_CAPACITY);
        let (data_tx, data_rx) = mpsc::channel(DATA_LANE_CAPACITY);
        let pipeline = Self {
            system_tx: system_tx.clone(),
            data_tx: Arc::new(Mutex::new(Some(data_tx.clone()))),
        };
        let ends = PipelineEnds {
            system_tx,
            data_tx,
            data_rx,
        };
        (pipeline, ends)
    }

    /// Push an interruption onto the system lane. Reaches every stage
    /// regardless of data-lane backlog.
    pub fn interrupt(&self) {
        let _ = self.system_tx.send(SystemFrame::Interruption);
    }

    pub fn user_started_speaking(&self) {
        let _ = self.system_tx.send(SystemFrame::UserStartedSpeaking);
    }

    pub fn user_stopped_speaking(&self) {
        let _ = self.system_tx.send(SystemFrame::UserStoppedSpeaking);
    }

    pub fn error(&self, msg: impl Into<String>) {
        let _ = self.system_tx.send(SystemFrame::Error(msg.into()));
    }

    /// Subscribe a fresh system-lane receiver. Every stage should own
    /// exactly one of these and poll it first in its `select!`.
    pub fn subscribe_system(&self) -> broadcast::Receiver<SystemFrame> {
        self.system_tx.subscribe()
    }

    /// Send a data-lane frame. Returns an error if the pipeline has
    /// been torn down.
    pub async fn emit(&self, frame: DataFrame) -> Result<(), String> {
        let tx = self.data_tx.lock().await;
        if let Some(tx) = tx.as_ref() {
            tx.send(frame)
                .await
                .map_err(|_| "data lane closed".to_string())
        } else {
            Err("pipeline torn down".to_string())
        }
    }

    /// Close the data lane. System-lane broadcasts still work.
    pub async fn close_data(&self) {
        let mut tx = self.data_tx.lock().await;
        *tx = None;
    }
}

/// The "receiving end" of the pipeline handed to the driver that
/// orchestrates stages.
pub struct PipelineEnds {
    pub system_tx: broadcast::Sender<SystemFrame>,
    pub data_tx: mpsc::Sender<DataFrame>,
    pub data_rx: mpsc::Receiver<DataFrame>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn system_lane_broadcasts_to_multiple_subscribers() {
        let (pipe, _ends) = Pipeline::new();
        let mut rx_a = pipe.subscribe_system();
        let mut rx_b = pipe.subscribe_system();

        pipe.interrupt();

        let got_a = tokio::time::timeout(Duration::from_millis(50), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        let got_b = tokio::time::timeout(Duration::from_millis(50), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got_a, SystemFrame::Interruption);
        assert_eq!(got_b, SystemFrame::Interruption);
    }

    #[tokio::test]
    async fn data_lane_is_fifo() {
        let (pipe, mut ends) = Pipeline::new();
        pipe.emit(DataFrame::TranscriptPartial("a".into()))
            .await
            .unwrap();
        pipe.emit(DataFrame::TranscriptPartial("b".into()))
            .await
            .unwrap();
        let f1 = ends.data_rx.recv().await.unwrap();
        let f2 = ends.data_rx.recv().await.unwrap();
        assert!(matches!(f1, DataFrame::TranscriptPartial(ref s) if s == "a"));
        assert!(matches!(f2, DataFrame::TranscriptPartial(ref s) if s == "b"));
    }

    /// The canonical ordering test: a system-lane interrupt fires
    /// WHILE the data lane has queued frames; the processor's
    /// `select!` must observe the interrupt before the queued frame.
    ///
    /// Note: `broadcast::Receiver` only sees messages sent AFTER it
    /// subscribes, so we subscribe first.
    #[tokio::test]
    async fn system_lane_preempts_data_lane_in_select_loop() {
        let (pipe, mut ends) = Pipeline::new();
        let mut sys_rx = pipe.subscribe_system();
        // Fill data lane first.
        for i in 0..10 {
            pipe.emit(DataFrame::LlmTokenDelta(format!("t{i}")))
                .await
                .unwrap();
        }
        pipe.interrupt();

        let result = tokio::select! {
            biased;
            sys = sys_rx.recv() => {
                Some(sys.unwrap())
            }
            _data = ends.data_rx.recv() => {
                None
            }
        };
        assert_eq!(
            result,
            Some(SystemFrame::Interruption),
            "biased select must observe system lane first"
        );
    }
}
