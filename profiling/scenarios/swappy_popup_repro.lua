local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

lum.on_startup(function()
	base.enable_virtual_output()
	lum.spawn({
		command = "bash",
		args = {
			"-lc",
			[[
mkdir -p /tmp/swappy-cfg
cat > /tmp/swappy-cfg/config <<'EOF'
[Default]
show_panel=true
paint_mode=brush
early_exit=false
EOF
export XDG_CONFIG_HOME=/tmp/swappy-cfg
export WAYLAND_DEBUG=1
exec swappy -f /tmp/lumalla-grim-out.png > /tmp/swappy-headless-out.log 2> /tmp/swappy-headless-err.log
]],
		},
	})
	-- Seconds (not ms). Hover toolbar long enough for GTK tooltips (xdg_popup).
	lum.sleep(1.5)
	for x = 40, 500, 30 do
		lum.pointer_move(x, 28)
		lum.sleep(0.35)
	end
	lum.sleep(0.5)
	lum.pointer_move(400, 400)
	lum.sleep(0.1)
	lum.click(400, 400, 273)
	lum.sleep(1.0)
	lum.quit()
end)
