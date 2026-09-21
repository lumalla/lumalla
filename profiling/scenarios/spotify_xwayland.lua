local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Headless CPU-profile load: xwayland-satellite :2 + Spotify on DISPLAY=:2.
-- `--mu` avoids clashing with any other Spotify singleton on the machine.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.spawn({ command = "xwayland-satellite", args = { ":2" } })
	lum.sleep(1.0)

	lum.set_extra_env("DISPLAY", ":2")
	lum.spawn({
		command = "spotify",
		args = { "--mu=cpu-profile" },
	})

	-- Steady-state window for ~2 minutes, then quit so perf record ends.
	lum.sleep(120)
	lum.quit()
end)
