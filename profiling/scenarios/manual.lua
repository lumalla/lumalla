local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

lum.on_startup(function()
	base.enable_all_outputs(lum.get_drm_devices())
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
end)
