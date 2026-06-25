fn main() {
    pkg_config::Config::new()
        .probe("libinput")
        .expect("pkg-config could not find libinput");
    pkg_config::Config::new()
        .probe("libudev")
        .expect("pkg-config could not find libudev");
}
