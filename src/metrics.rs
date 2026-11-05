use crate::Error;
use kube::ResourceExt;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use tokio::time::Instant;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct KindLabels {
    kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FailureLabels {
    kind: String,
    instance: String,
    error: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CrdLabels {
    crd: String,
}

type DurationFamily = Family<KindLabels, Histogram, fn() -> Histogram>;

#[derive(Clone)]
pub struct Metrics {
    reconciliations: Counter,
    failures: Family<FailureLabels, Counter>,
    reconcile_duration: DurationFamily,
    /// 1 while a controller is waiting for its CRD to show up, 0 once it is available.
    ///
    /// A CRD that never arrives leaves the operator waiting forever, which is otherwise
    /// only visible in the logs; this makes it alertable.
    crd_missing: Family<CrdLabels, Gauge>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            reconciliations: Counter::default(),
            failures: Family::default(),
            reconcile_duration: Family::new_with_constructor(|| {
                Histogram::new([0.01, 0.1, 0.25, 0.5, 1., 5., 15., 60.])
            }),
            crd_missing: Family::default(),
        }
    }
}

impl Metrics {
    /// Register API metrics to start tracking them.
    ///
    /// Counters are registered without their `_total` suffix, which the encoder appends.
    pub fn register(self, registry: &mut Registry) -> Self {
        registry.register(
            "restate_operator_reconcile_duration_seconds",
            "The duration of reconcile to complete in seconds",
            self.reconcile_duration.clone(),
        );
        registry.register(
            "restate_operator_reconciliation_errors",
            "reconciliation errors",
            self.failures.clone(),
        );
        registry.register(
            "restate_operator_reconciliations",
            "reconciliations",
            self.reconciliations.clone(),
        );
        registry.register(
            "restate_operator_crd_missing",
            "1 while the operator is waiting for a CRD to appear in the apiserver's discovery information, 0 once it is available",
            self.crd_missing.clone(),
        );
        self
    }

    pub fn reconcile_failure<T: kube::Resource<DynamicType = ()>>(&self, rc: &T, e: &Error) {
        self.failures
            .get_or_create(&FailureLabels {
                kind: T::kind(&()).into_owned(),
                instance: rc.name_any(),
                error: e.metric_label().to_owned(),
            })
            .inc();
    }

    pub fn crd_missing_gauge(&self, crd: &str) -> Gauge {
        self.crd_missing
            .get_or_create(&CrdLabels {
                crd: crd.to_owned(),
            })
            .clone()
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
    metric: DurationFamily,
    _resource_type: std::marker::PhantomData<T>,
}

impl<T: kube::Resource<DynamicType = ()>> Drop for ReconcileMeasurer<T> {
    fn drop(&mut self) {
        #[allow(clippy::cast_precision_loss)]
        let duration = self.start.elapsed().as_millis() as f64 / 1000.0;
        self.metric
            .get_or_create(&KindLabels {
                kind: T::kind(&()).into_owned(),
            })
            .observe(duration);
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::ConfigMap;

    use super::*;

    /// Dashboards and alerts query these names, so they must survive a metrics library change.
    #[test]
    fn exposes_the_established_sample_names() {
        let mut registry = Registry::default();
        let metrics = Metrics::default().register(&mut registry);
        let cm = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some("cm".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        drop(metrics.count_and_measure::<ConfigMap>());
        metrics.reconcile_failure(&cm, &Error::Provisioned);
        metrics
            .crd_missing_gauge("restateclusters.restate.dev")
            .set(1);

        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &registry).unwrap();

        for sample in [
            "restate_operator_reconciliations_total 1",
            r#"restate_operator_reconciliation_errors_total{kind="ConfigMap",instance="cm",error="Provisioned"} 1"#,
            r#"restate_operator_reconcile_duration_seconds_bucket{le="0.01",kind="ConfigMap"} 1"#,
            r#"restate_operator_reconcile_duration_seconds_count{kind="ConfigMap"} 1"#,
            r#"restate_operator_crd_missing{crd="restateclusters.restate.dev"} 1"#,
        ] {
            assert!(text.contains(sample), "missing {sample} in:\n{text}");
        }
    }
}
