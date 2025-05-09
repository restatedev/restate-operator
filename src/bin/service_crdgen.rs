use kube::CustomResourceExt;

fn main() {
    print!(
        "{}",
        serde_yaml::to_string(
            &restate_operator::resources::restatedeployments::RestateDeployment::crd()
        )
        .unwrap()
    )
}
