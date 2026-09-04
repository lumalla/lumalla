local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Launch qalculate-qt, focus the expression field, evaluate 1+1, then exit.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.spawn({ command = "qalculate-qt" })
	lum.sleep(2)

	local x, y = base.point_on_primary(640, 180)
	lum.click(x, y)
	lum.sleep(0.2)

	lum.type("1+1")
	lum.key("Return")
	lum.sleep(1)

	lum.quit()
end)
