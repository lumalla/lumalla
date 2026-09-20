-- Print env and keep compositor up so we can attach lumalla-ui manually.
local lum = require("lumalla")
package.path = package.path .. ";./profiling/lib/?.lua"
local base = require("base")

lum.on_startup(function()
	base.enable_virtual_output()
	print("WAYLAND_DISPLAY=" .. tostring(os.getenv("WAYLAND_DISPLAY")))
	print("outputs=" .. tostring(#lum.get_outputs()))
	-- Stay alive for manual testing.
	lum.sleep(30)
	lum.quit()
end)
