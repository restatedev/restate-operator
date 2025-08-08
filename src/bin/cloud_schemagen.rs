use schemars::JsonSchema;
fn main() {
    let mut gen = schemars::gen::SchemaSettings::openapi3()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .with_visitor(kube::core::schema::StructuralSchemaRewriter)
        .into_generator();
    print!(
        "{}",
        serde_json::to_string_pretty(
            &restate_operator::resources::restatecloudenvironments::RestateCloudEnvironment::json_schema(
                &mut gen
            )
        )
        .unwrap()
    )
}
