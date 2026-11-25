// The WebGPU build of ONNX Runtime links Dawn (libwebgpu_dawn.dylib) as
// @rpath/libwebgpu_dawn.dylib. Look for it next to each binary: in the VST3
// bundle that's Contents/MacOS (scripts/install.sh copies it there); for
// tests and examples, ort copies it next to them in target/.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
    }
}
