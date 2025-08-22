<div align="center">
  <a href="https://lumalla.org">
    <img src="assets/logo.svg" alt="Logo" width="80" height="80">
  </a>

  <h3 align="center">Lumalla</h3>

  <p align="center">
    Window manager focused on configurability.
  </p>
</div>

## Architecture

### Main

Reads args and determines if it should start the window manager or connect to an already running instance.

### Config

Reads the main config file and determines how to configure the seat, input, rendering and display handling.

### Input

Gathers all input events and forwards them to the config/display.

### Display

Handles connections to the clients and organizes the window layout.

### Renderer

Renders the windows and other elements based on the display layout.

## License

Except where noted, all code in this repository is dual-licensed under either:

* MIT License ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

at your option. This means you can select the license you prefer!

### Your contributions

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you,
as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
