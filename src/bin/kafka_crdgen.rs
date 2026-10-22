use kube::CustomResourceExt;

fn main() {
    print!(
        "{}",
        serde_yaml::to_string(
            &restate_operator::resources::restatekafkaintegrations::RestateKafkaIntegration::crd()
        )
        .unwrap()
    )
}
