local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Bring outputs up and keep the compositor running until quit is requested.
lum.on_startup(function()
	base.enable_all_outputs(lum.get_drm_devices())
	lum.spawn({ command = "qalculate-qt" })
end)
