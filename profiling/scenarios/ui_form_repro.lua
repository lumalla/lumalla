-- Headless repro: open lum.ui, then kill the helper after a moment by quitting.
-- Success means the form stayed open (no immediate on_cancel from a crash).
local lum = require("lumalla")

package.path = package.path .. ";./profiling/lib/?.lua"
local base = require("base")

local opened = false
local cancelled_early = false

lum.on_startup(function()
	base.enable_virtual_output()
	print("ui_form_repro: WAYLAND_DISPLAY=" .. tostring(os.getenv("WAYLAND_DISPLAY")))
	print("ui_form_repro: calling lum.ui()")
	local ok, err = pcall(function()
		lum.ui({
			title = "New view",
			fields = {
				{ id = "name", type = "text", label = "Name", placeholder = "pip", focus = true },
			},
			actions = {
				{ id = "cancel", label = "Cancel" },
				{ id = "ok", label = "Create", primary = true, submit = true },
			},
			on_submit = function(values, action)
				print("ui_form_repro: on_submit action=" .. tostring(action) .. " name=" .. tostring(values.name))
				lum.quit()
			end,
			on_cancel = function()
				if not opened then
					cancelled_early = true
					print("ui_form_repro: FAIL early on_cancel (helper crashed?)")
				else
					print("ui_form_repro: on_cancel after open")
				end
				lum.quit()
			end,
		})
	end)
	if not ok then
		print("ui_form_repro: FAIL pcall: " .. tostring(err))
		lum.quit()
		return
	end
	-- Give the helper time to map a window. If it crashed immediately, on_cancel
	-- already ran. Otherwise mark opened and quit (kills session / helper).
	lum.sleep(1.5)
	if cancelled_early then
		return
	end
	opened = true
	print("ui_form_repro: OK form still open after 1.5s")
	lum.quit()
end)
