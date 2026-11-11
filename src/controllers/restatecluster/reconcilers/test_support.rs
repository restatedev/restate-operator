use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kube::runtime::reflector;

use crate::Metrics;
use crate::controllers::State;
use crate::controllers::restatecluster::controller::Context;

/// A context whose apiserver fails every request, along with how many it received. The
/// SecretProviderClass CRD counts as installed, so only the switches keep requests away.
pub fn context_with_failing_client(
    manage_network_policies: bool,
    manage_secret_provider_classes: bool,
) -> (Arc<Context>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let service = tower::service_fn(move |_: http::Request<kube::client::Body>| {
        counter.fetch_add(1, Ordering::SeqCst);
        async {
            Err::<http::Response<kube::client::Body>, _>(std::io::Error::other("no apiserver"))
        }
    });
    let state = State::new(
        None,
        false,
        manage_network_policies,
        manage_secret_provider_classes,
        "restate-operator".into(),
        None,
        None,
        "tunnel:latest".into(),
        "cluster.local".into(),
        "alpine:3.21".into(),
        None,
        None,
    );
    let ctx = Context::new(
        kube::Client::new(service, "default"),
        Metrics::default(),
        state,
        reflector::store().0,
        reflector::store().0,
        false,
        true,
    );
    (ctx, requests)
}
