fn main() {
    pkg_config::Config::new()
        .probe("libinput")
        .expect("pkg-config could not find libinput");
    pkg_config::Config::new()
        .probe("xkbcommon")
        .expect("pkg-config could not find xkbcommon");
}
