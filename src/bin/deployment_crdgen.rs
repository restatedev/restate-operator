fn main() {
    print!(
        "{}",
        restate_operator::resources::crd_yaml::<
            restate_operator::resources::restatedeployments::RestateDeployment,
        >()
    )
}
