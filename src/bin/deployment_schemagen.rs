use schemars::JsonSchema;
fn main() {
    let mut generator = schemars::generate::SchemaSettings::openapi3()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .with_transform(kube::core::schema::StructuralSchemaRewriter)
        .into_generator();
    print!(
        "{}",
        serde_json::to_string_pretty(
            &restate_operator::resources::restatedeployments::RestateDeployment::json_schema(
                &mut generator
            )
        )
        .unwrap()
    )
}
