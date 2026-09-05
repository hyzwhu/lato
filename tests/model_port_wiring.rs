#[test]
fn cli_provider_constructors_cross_the_canonical_model_port_boundary() {
    let source = include_str!("../src/cli.rs");
    let boundary_call = ["adapt_model_endpoint", "(provider, model_id"].concat();
    assert_eq!(source.matches(&boundary_call).count(), 3);
}

#[test]
fn acp_provider_constructors_cross_the_canonical_model_port_boundary() {
    let source = include_str!("../crates/lato-agent/src/host.rs");
    let boundary_call = ["adapt_model_endpoint", "(provider, model"].concat();
    assert_eq!(source.matches(&boundary_call).count(), 2);
}
