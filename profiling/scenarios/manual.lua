local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

lum.on_startup(function()
	base.enable_virtual_output()
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
end)
