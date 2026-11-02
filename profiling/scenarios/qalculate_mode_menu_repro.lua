local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Reproduce: qalculate-qt Mode (Alt+M) while a completion popup is open.
-- Qt parents the Mode menu to the completion popup with a negative
-- anchor_rect; lumalla rejects that as xdg InvalidPositioner.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.add_window_rule({
		app_id = "io.github.Qalculate.qalculate-qt",
		x = 80,
		y = 80,
		width = 640,
		height = 420,
	})

	lum.spawn({
		command = "bash",
		args = {
			"-lc",
			[[
export WAYLAND_DEBUG=1
export HOME=/tmp/qalc-lumalla-home
mkdir -p "$HOME"
exec qalculate-qt > /tmp/qalc-mode-repro.out 2> /tmp/qalc-mode-repro.err
]],
		},
	})
	lum.sleep(2.5)

	-- Open completion popup, then Mode via Alt+M.
	local fx, fy = base.point_on_primary(200, 300)
	lum.click(fx, fy)
	lum.sleep(0.2)
	lum.type("sin")
	lum.sleep(0.8)
	lum.key("Alt+m")
	lum.sleep(1.0)

	lum.quit()
end)

