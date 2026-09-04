local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Calculator + terminal: click each and type a unique marker.
-- Exclusive seat keyboard focus means each marker appears in only one window.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.add_window_rule({
		app_id = "io.github.Qalculate.qalculate-qt",
		x = 80,
		y = 80,
		width = 520,
		height = 600,
	})
	lum.add_window_rule({
		app_id = "org.wezfurlong.wezterm",
		x = 720,
		y = 80,
		width = 700,
		height = 500,
	})

	lum.spawn({ command = "qalculate-qt" })
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	lum.sleep(3.0)

	local ax, ay = base.point_on_primary(340, 220)
	local bx, by = base.point_on_primary(1070, 280)

	lum.click(ax, ay)
	lum.sleep(0.4)
	lum.type("FOCUS_A")
	lum.sleep(0.6)

	lum.click(bx, by)
	lum.sleep(0.4)
	lum.type("FOCUS_B")
	lum.sleep(0.8)

	local out = base.primary_output()
	local w = out and out.width or 1920
	local h = out and out.height or 1080
	-- Capture the region covering both windows.
	lum.screenshot(
		(out and out.x or 0) + 60,
		(out and out.y or 0) + 60,
		1400,
		700,
		"/tmp/lumalla-keyboard-focus.png"
	)
	lum.sleep(0.5)
	lum.quit()
end)
