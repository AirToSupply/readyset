//! Support for recording and exporting in-memory metrics using the [`metrics`] crate

use crossbeam::queue::ArrayQueue;
use metrics::{GaugeValue, Key, Recorder, SetRecorderError, Unit};
use std::collections::HashMap;
use std::mem;
use std::sync::Mutex;

pub use noria::metrics::MetricsDump;

static mut METRICS_RECORDER: Option<NoriaMetricsRecorder> = None;

enum MetricsOp {
    IncrementCounter(Key, u64),
    UpdateGauge(Key, GaugeValue),
}

type OpQueue = ArrayQueue<MetricsOp>;

#[derive(Default)]
struct MetricsInner {
    counters: HashMap<Key, u64>,
    gauges: HashMap<Key, f64>,
}

impl MetricsInner {
    fn new() -> Self {
        Default::default()
    }

    fn collapse(&mut self, queue: &OpQueue) {
        while let Some(op) = queue.pop() {
            match op {
                MetricsOp::IncrementCounter(k, v) => {
                    let ent = self.counters.entry(k).or_default();
                    *ent += v;
                }
                MetricsOp::UpdateGauge(k, v) => {
                    let ent = self.gauges.entry(k).or_default();
                    match v {
                        GaugeValue::Increment(v) => {
                            *ent += v;
                        }
                        GaugeValue::Absolute(v) => {
                            *ent = v;
                        }
                        GaugeValue::Decrement(v) => {
                            *ent -= v;
                        }
                    }
                }
            }
        }
    }
}

/// A simplistic metrics recorder for Noria, based on an operation queue and mutexed hashmaps.
pub struct NoriaMetricsRecorder {
    inner: Mutex<MetricsInner>,
    queue: OpQueue,
}

impl NoriaMetricsRecorder {
    /// Adds a `MetricsOp` to the operation queue. If the operation queue is full,
    /// locks the mutex and collapses the operation queue into `inner`.
    ///
    /// This design should mean that metrics are pretty lightweight on average, with the occasional
    /// spike.
    fn push_op(&self, op: MetricsOp) {
        if let Err(op) = self.queue.push(op) {
            {
                let mut inner = self.inner.lock().unwrap();
                inner.collapse(&self.queue);
            }
            if let Err(_) = self.queue.push(op) {
                eprintln!("WARNING: failed to push metrics op after a collapse!");
            }
        }
    }

    /// Makes and installs a new `NoriaMetricsRecorder`, with the metrics queue capacity set to
    /// `cap`.
    ///
    /// # Safety
    ///
    /// This function is unsafe to call when there are multiple threads; it MUST be called before
    /// other threads are created.
    pub unsafe fn install(cap: usize) -> Result<(), SetRecorderError> {
        let rec = NoriaMetricsRecorder {
            inner: Mutex::new(MetricsInner::new()),
            queue: ArrayQueue::new(cap),
        };
        if mem::replace(&mut METRICS_RECORDER, Some(rec)).is_some() {
            panic!("noria metrics recorder installed twice!")
        }
        metrics::set_recorder_racy(METRICS_RECORDER.as_ref().unwrap())
    }

    /// Gets a static reference to the installed metrics recorder.
    ///
    /// # Panics
    ///
    /// This method panics if `install()` has not been called yet.
    pub fn get() -> &'static Self {
        // SAFETY: no data races possible, since METRICS_RECORDER is only mutated once (and the
        // `install()` function is marked `unsafe`).
        unsafe {
            METRICS_RECORDER
                .as_ref()
                .expect("noria metrics recorder not installed yet")
        }
    }

    /// Runs `func` with two arguments: a map of counters and a map of gauges.
    ///
    /// This collapses the metrics queue before running the supplied function.
    pub fn with_metrics<F, R>(&self, func: F) -> R
    where
        F: FnOnce(&HashMap<Key, u64>, &HashMap<Key, f64>) -> R,
    {
        let mut inner = self.inner.lock().unwrap();
        inner.collapse(&self.queue);
        func(&inner.counters, &inner.gauges)
    }
}

impl Recorder for NoriaMetricsRecorder {
    fn register_counter(&self, _key: Key, _unit: Option<Unit>, _description: Option<&'static str>) {
        // no-op
    }

    fn register_gauge(&self, _key: Key, _unit: Option<Unit>, _description: Option<&'static str>) {
        // no-op
    }

    fn register_histogram(
        &self,
        _key: Key,
        _unit: Option<Unit>,
        _description: Option<&'static str>,
    ) {
        unimplemented!("histogram metrics are not supported yet")
    }

    fn increment_counter(&self, key: Key, value: u64) {
        self.push_op(MetricsOp::IncrementCounter(key, value))
    }

    fn update_gauge(&self, key: Key, value: GaugeValue) {
        self.push_op(MetricsOp::UpdateGauge(key, value))
    }

    fn record_histogram(&self, _key: Key, _value: f64) {
        unimplemented!("histogram metrics are not supported yet")
    }
}
