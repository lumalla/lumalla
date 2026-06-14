local lum = require("lumalla")

lum.init({
	renderer = "native",
	seat = "native",
	wayland_socket = "$XDG_RUNTIME_DIR/",
})
