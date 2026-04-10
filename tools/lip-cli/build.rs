fn main() {
    println!("cargo:rerun-if-changed=src/proto/scip.proto");

    // Use the vendored protoc binary so no system install is needed.
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored: no binary available for this platform");
    // SAFETY: build scripts run single-threaded before any user code.
    unsafe { std::env::set_var("PROTOC", protoc) };

    prost_build::compile_protos(&["src/proto/scip.proto"], &["src/proto/"])
        .expect("prost-build failed to compile scip.proto");
}
