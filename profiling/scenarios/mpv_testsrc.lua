local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Continuous small-region damage: looping lavfi test pattern in a tiny window.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.spawn({
		command = "mpv",
		args = {
			"--vo=dmabuf-wayland",
			"--geometry=160x160+40+40",
			"--loop=inf",
			"--no-border",
			"--no-osc",
			"--really-quiet",
			"--hwdec=no",
			"--force-window=yes",
			"av://lavfi:testsrc2=size=160x160:rate=60",
		},
	})

	lum.sleep(120)
	lum.quit()
end)
