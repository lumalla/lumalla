local lum = require("lumalla")
local base = dofile("profiling/lib/base.lua")

-- Reproduce pointer edge protocol issues with two stacked wezterms.
lum.on_startup(function()
	base.enable_virtual_output()

	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	lum.sleep(1.8)
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	lum.sleep(2.0)

	local windows = lum.get_windows()
	io.stderr:write(string.format("windows=%d\n", #windows))
	for _, w in ipairs(windows) do
		io.stderr:write(
			string.format(
				"  id=%s focused=%s geom=%d,%d %dx%d\n",
				tostring(w.id),
				tostring(w.focused),
				w.x,
				w.y,
				w.width,
				w.height
			)
		)
	end
	if #windows < 2 then
		lum.quit()
		return
	end

	table.sort(windows, function(a, b)
		return a.id < b.id
	end)
	local back = windows[1]
	local front = windows[2]

	-- Keep cascade positions; probe exclusive front strips and the overlap.
	local probes = {
		{ "outside_left", back.x - 20, back.y + 40 },
		{ "back_only", back.x + 10, back.y + 10 },
		{ "overlap", front.x + 40, front.y + 40 },
		{ "front_right_exclusive", back.x + back.width + 10, front.y + math.floor(front.height / 2) },
		{ "front_bottom_exclusive", front.x + 80, back.y + back.height + 10 },
		{ "front_top_edge", front.x + 80, front.y + 4 },
		{ "front_left_edge", front.x + 4, front.y + 80 },
		{ "outside_right", front.x + front.width + 20, front.y + 80 },
	}

	for _, p in ipairs(probes) do
		local name, x, y = p[1], p[2], p[3]
		io.stderr:write(string.format("probe %s -> %d,%d\n", name, x, y))
		lum.pointer_move(x, y)
		lum.sleep(0.15)
		-- Micro-sweep to force motion events after enter.
		lum.pointer_move(x + 2, y + 2)
		lum.sleep(0.1)
	end

	io.stderr:write("click overlap (expect front focused)\n")
	lum.click(front.x + 50, front.y + 50)
	lum.sleep(0.2)
	io.stderr:write(string.format("focused=%s front=%s\n", tostring(lum.get_focused_window()), tostring(front.id)))

	io.stderr:write("click back-only corner (expect back focused)\n")
	lum.click(back.x + 10, back.y + 10)
	lum.sleep(0.2)
	io.stderr:write(string.format("focused=%s back=%s\n", tostring(lum.get_focused_window()), tostring(back.id)))

	-- Return to exclusive front edge (the user-reported failure region).
	lum.pointer_move(back.x + back.width + 10, front.y + math.floor(front.height / 2))
	lum.sleep(0.4)

	local out = base.primary_output()
	lum.screenshot(
		(out and out.x or 0),
		(out and out.y or 0),
		1200,
		900,
		"/tmp/lumalla-wezterm-stack-edge.png"
	)
	lum.sleep(0.4)
	lum.quit()
end)
