use kube::CustomResourceExt;
fn main() {
    print!(
        "{}",
        serde_yaml::to_string(&kube_controllers::RestateCluster::crd()).unwrap()
    )
}
