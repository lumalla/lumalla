-- Minimal config for PipeWire window stream smoke test.
-- Starts a region stream when no windows exist yet, then also exercises the
-- window API (expects failure without a focused window — prints status).
local lum = require("lumalla")

lum.on_startup(function()
	lum.add_output({
		name = "VIRTUAL-1",
		description = "Lumalla virtual output",
		width = 640,
		height = 480,
		refresh_mhz = 60000,
		mm_width = 160,
		mm_height = 120,
		scale = 1,
		virtual = true,
	})
	lum.add_view("VIRTUAL-1", {
		name = "main",
		source_x = 0,
		source_y = 0,
		source_width = 640,
		source_height = 480,
		dest_x = 0,
		dest_y = 0,
		dest_width = 640,
		dest_height = 480,
	})

	local ok, stream_or_err = pcall(function()
		return lum.start_pipewire_stream_window({
			name = "LumallaWindowTest",
			max_fps = 10,
		})
	end)
	if ok then
		print(string.format(
			"PIPEWIRE_WINDOW_OK id=%s node_id=%s",
			stream_or_err.id,
			stream_or_err.node_id
		))
	else
		-- Expected when no client windows are mapped yet.
		print("PIPEWIRE_WINDOW_NO_FOCUS " .. tostring(stream_or_err))
	end
	lum.quit()
end)
