use std::path::Path;

fn main() {
    let schema = Path::new("../../schema/lip.fbs");
    println!("cargo:rerun-if-changed={}", schema.display());
    println!("cargo:rerun-if-changed=build.rs");

    match which::which("flatc") {
        Ok(flatc) => {
            let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
            let status = std::process::Command::new(flatc)
                .args([
                    "--rust",
                    "--gen-all",
                    "-o",
                    &out_dir,
                    schema.to_str().unwrap(),
                ])
                .status()
                .expect("failed to run flatc");
            if !status.success() {
                eprintln!("cargo:warning=flatc exited with status {status}; falling back to hand-written types");
            } else {
                println!("cargo:rustc-cfg=feature=\"flatc_generated\"");
            }
        }
        Err(_) => {
            eprintln!("cargo:warning=flatc not found; FlatBuffers zero-copy reading disabled (IPC uses JSON in v0.1)");
        }
    }
}
