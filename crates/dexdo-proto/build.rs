fn main() {
    // Cross-platform build(D6): if PROTOC is not set in the environment, we substitute
    // the vendored protoc binary -- the build does not require a system protoc on Linux/macOS/Windows.
    // An explicitly set PROTOC(e.g. from the build environment) is respected and takes priority.
    if std::env::var_os("PROTOC").is_none() {
        if let Ok(protoc) = protoc_bin_vendored::protoc_bin_path() {
            std::env::set_var("PROTOC", protoc);
        }
    }
    tonic_build::compile_protos("proto/dexdo.proto").expect("compile dexdo.proto");
}
