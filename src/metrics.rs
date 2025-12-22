use crate::Error;
use kube::ResourceExt;
use prometheus::{HistogramVec, IntCounter, IntCounterVec, Registry, histogram_opts, opts};
use tokio::time::Instant;

#[derive(Clone)]
pub struct Metrics {
    pub reconciliations: IntCounter,
    pub failures: IntCounterVec,
    pub reconcile_duration: HistogramVec,
}

impl Default for Metrics {
    fn default() -> Self {
        let reconcile_duration = HistogramVec::new(
            histogram_opts!(
                "restate_operator_reconcile_duration_seconds",
                "The duration of reconcile to complete in seconds"
            )
            .buckets(vec![0.01, 0.1, 0.25, 0.5, 1., 5., 15., 60.]),
            &["kind"],
        )
        .unwrap();
        let failures = IntCounterVec::new(
            opts!(
                "restate_operator_reconciliation_errors_total",
                "reconciliation errors",
            ),
            &["kind", "instance", "error"],
        )
        .unwrap();
        let reconciliations =
            IntCounter::new("restate_operator_reconciliations_total", "reconciliations").unwrap();
        Metrics {
            reconciliations,
            failures,
            reconcile_duration,
        }
    }
}

impl Metrics {
    /// Register API metrics to start tracking them.
    pub fn register(self, registry: &Registry) -> Result<Self, prometheus::Error> {
        registry.register(Box::new(self.reconcile_duration.clone()))?;
        registry.register(Box::new(self.failures.clone()))?;
        registry.register(Box::new(self.reconciliations.clone()))?;
        Ok(self)
    }

    pub fn reconcile_failure<T: kube::Resource<DynamicType = ()>>(&self, rc: &T, e: &Error) {
        self.failures
            .with_label_values(&[
                T::kind(&()).as_ref(),
                rc.name_any().as_ref(),
                e.metric_label(),
            ])
            .inc()
    }

    pub fn count_and_measure<T: kube::Resource<DynamicType = ()>>(&self) -> ReconcileMeasurer<T> {
        self.reconciliations.inc();
        ReconcileMeasurer {
            start: Instant::now(),
            metric: self.reconcile_duration.clone(),
            _resource_type: std::marker::PhantomData,
        }
    }
}

/// Smart function duration measurer
///
/// Relies on Drop to calculate duration and register the observation in the histogram
pub struct ReconcileMeasurer<T: kube::Resource<DynamicType = ()>> {
    start: Instant,
    metric: HistogramVec,
    _resource_type: std::marker::PhantomData<T>,
}

impl<T: kube::Resource<DynamicType = ()>> Drop for ReconcileMeasurer<T> {
    fn drop(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let duration = self.start.elapsed().as_millis() as f64 / 1000.0;
        self.metric
            .with_label_values(&[T::kind(&()).as_ref()])
            .observe(duration);
    }
}
