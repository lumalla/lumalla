fn main() {
    pkg_config::Config::new()
        .probe("libudev")
        .expect("pkg-config could not find libudev");
}
