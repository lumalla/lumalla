fn main() {
    pkg_config::Config::new()
        .probe("libdrm")
        .expect("pkg-config could not find libdrm");
}
