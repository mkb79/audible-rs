//! Build script: generate Rust types for the Widevine license protocol
//! (AUD-56) from `proto/license_protocol.proto`.
//!
//! Uses `protox` (a pure-Rust protobuf compiler) so no native `protoc` is
//! required. Only `prost` (the small runtime) is linked into the binary; the
//! codegen tooling here is build-time only.

fn main() {
    let proto = "proto/license_protocol.proto";
    let descriptors = protox::compile([proto], ["proto"]).expect("compile license_protocol.proto");
    prost_build::Config::new()
        .compile_fds(descriptors)
        .expect("generate prost types");
    println!("cargo:rerun-if-changed={proto}");
}
