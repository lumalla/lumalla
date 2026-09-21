local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Continuous small-region damage via weston's smoke client (frame-callback paced).
-- Note: weston-flower only paints once on map; use smoke for steady animation.
lum.on_startup(function()
	base.enable_virtual_output()

	-- Prefer WESTON_SMOKE if set (absolute store path); else PATH lookup.
	local smoke = os.getenv("WESTON_SMOKE") or "weston-smoke"
	lum.spawn({ command = smoke })

	lum.sleep(120)
	lum.quit()
end)
