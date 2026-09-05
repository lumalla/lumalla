local lum = require("lumalla")

--- Default headless / non-TTY output. For real DRM connectors, call
--- enable_all_drm_outputs from profiling/lib/base.lua instead.
local function enable_virtual_output()
	for _, output in ipairs(lum.get_outputs()) do
		if output.name == "VIRTUAL-1" then
			return
		end
	end
	lum.add_output({
		name = "VIRTUAL-1",
		description = "Lumalla virtual output",
		x = 0,
		y = 0,
		width = 1920,
		height = 1080,
		refresh_mhz = 60000,
		mm_width = 300,
		mm_height = 200,
		scale = 1,
		virtual = true,
	})
end

lum.on_startup(function()
	enable_virtual_output()
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	-- lum.spawn({ command = "xwayland-satellite", args = { ":12" } })
	-- lum.sleep(2.0)
	-- lum.spawn({
	-- 	command = "env",
	-- 	args = { "DISPLAY=:12", "steam" },
	-- })
end)

lum.add_window_rule({
	app_id = "io.github.Qalculate.qalculate-qt",
	x = 1400,
	y = 100,
	width = 400,
	height = 600,
})
-- Call set_xkb before map_key so binding key names resolve against the active layout.
lum.set_xkb({
	layout = "de",
})
lum.map_key({
	key = "m",
	mods = "logo",
	callback = function()
		lum.set_window({ width = 1200, height = 800 })
	end,
})
