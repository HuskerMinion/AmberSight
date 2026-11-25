// The helper's version is AmberSight's, read from the root Cargo.toml, and
// the program looks for libwebgpu_dawn.so beside itself.
fn main() {
    let toml = std::fs::read_to_string("../../Cargo.toml").expect("root Cargo.toml");
    let v = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \"").and_then(|r| r.strip_suffix('"')))
        .expect("version in root Cargo.toml");
    println!("cargo:rustc-env=AMBERSIGHT_VERSION={v}");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
}
