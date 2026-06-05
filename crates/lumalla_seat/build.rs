fn main() {
    pkg_config::Config::new()
        .probe("libseat")
        .expect("pkg-config could not find libseat");
}
