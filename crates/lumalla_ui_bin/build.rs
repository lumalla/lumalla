//! Link Wayland client libs so runtime `dlopen` from wayland-sys succeeds.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("linux") {
        return;
    }
    if pkg_config::probe_library("wayland-client").is_ok() {
        let _ = pkg_config::probe_library("wayland-cursor");
        let _ = pkg_config::probe_library("wayland-egl");
        return;
    }
    println!("cargo:rustc-link-lib=wayland-client");
    println!("cargo:rustc-link-lib=wayland-cursor");
    println!("cargo:rustc-link-lib=wayland-egl");
}
