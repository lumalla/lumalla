local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Outputs up, no client — for verifying the compositor sleeps when idle.
lum.on_startup(function()
	base.enable_all_outputs(lum.get_drm_devices())
end)
