fn main() {
    // Cross-platform build (D6): if PROTOC is not set in the environment, we substitute
    // the vendored protoc binary -- the build does not require a system protoc on Linux/macOS/Windows.
    // An explicitly set PROTOC (e.g. from the build environment) is respected and takes priority.
    if std::env::var_os("PROTOC").is_none() {
        if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
            std::env::set_var("PROTOC", protoc);
        }
    }
    tonic_build::configure()
        .type_attribute(
            ".dexdo.v1.BillingUsage",
            "#[derive(serde::Serialize, serde::Deserialize)]",
        )
        .field_attribute(
            ".dexdo.v1.BillingUsage.cumulative_billable_tokens",
            "#[serde(default, skip_serializing_if = \"Option::is_none\")]",
        )
        .compile_protos(&["proto/dexdo.proto"], &["proto"])
        .expect("compile dexdo.proto");
}
