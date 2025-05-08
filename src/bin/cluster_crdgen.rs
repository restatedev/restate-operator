use kube::CustomResourceExt;
fn main() {
    print!(
        "{}",
        serde_yaml::to_string(&restate_operator::resources::restateclusters::RestateCluster::crd())
            .unwrap()
    )
}
