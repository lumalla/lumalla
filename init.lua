local lum = require("lumalla")

--- Prefer the first connected DRM connector; fall back to a virtual output when
--- running headless / without a seat.
local function preferred_mode(connector)
	for _, mode in ipairs(connector.modes or {}) do
		if mode.preferred then
			return mode
		end
	end
	return (connector.modes or {})[1]
end

local function first_connected_connector(devices)
	local connectors = {}
	for _, device in ipairs(devices or {}) do
		for _, connector in ipairs(device.connectors or {}) do
			if connector.connected then
				table.insert(connectors, connector)
			end
		end
	end
	table.sort(connectors, function(a, b)
		return a.name < b.name
	end)
	return connectors[1]
end

local function primary_output()
	for _, output in ipairs(lum.get_outputs()) do
		if not output.virtual then
			return output
		end
	end
	return lum.get_outputs()[1]
end

local function enable_outputs()
	local connector = first_connected_connector(lum.get_drm_devices())
	if connector then
		local mode = preferred_mode(connector)
		local width = mode and mode.width or 0
		local height = mode and mode.height or 0
		if width > 0 and height > 0 then
			lum.set_output_configs({
				{
					name = connector.name,
					enabled = true,
					mode = mode and mode.name or nil,
				},
			})
			for _, output in ipairs(lum.get_outputs()) do
				if output.name == connector.name then
					return output
				end
			end
			lum.add_output({
				name = connector.name,
				description = connector.connector_type .. " " .. connector.name,
				width = width,
				height = height,
				refresh_mhz = mode and (mode.refresh_hz * 1000) or 60000,
				mm_width = connector.mm_width,
				mm_height = connector.mm_height,
				scale = 1,
				virtual = false,
			})
			return {
				name = connector.name,
				width = width,
				height = height,
				x = 0,
				y = 0,
				virtual = false,
			}
		end
	end

	for _, output in ipairs(lum.get_outputs()) do
		if output.name == "VIRTUAL-1" then
			return output
		end
	end
	lum.add_output({
		name = "VIRTUAL-1",
		description = "Lumalla virtual output",
		width = 1920,
		height = 1080,
		refresh_mhz = 60000,
		mm_width = 300,
		mm_height = 200,
		scale = 1,
		virtual = true,
	})
	return {
		name = "VIRTUAL-1",
		width = 1920,
		height = 1080,
		x = 0,
		y = 0,
		virtual = true,
	}
end

--- Active view names on the demo output (remove before applying a preset).
local active_views = {}

--- Tracked "main" camera into the scene (source rect). Updated by presets and view-move mode.
local main_view = nil

local function clear_views(output_name)
	for _, name in ipairs(active_views) do
		pcall(lum.remove_view, output_name, name)
	end
	active_views = {}
end

local function set_views(output_name, views)
	clear_views(output_name)
	for _, view in ipairs(views) do
		lum.add_view(output_name, view)
		table.insert(active_views, view.name)
		if view.name == "main" and view.source then
			main_view = {
				x = view.source.x,
				y = view.source.y,
				w = view.source.width,
				h = view.source.height,
			}
		end
	end
end

local function push_main_view()
	local output = primary_output()
	if not output or not main_view then
		return
	end
	lum.add_view(output.name, {
		name = "main",
		source = {
			x = math.floor(main_view.x + 0.5),
			y = math.floor(main_view.y + 0.5),
			width = math.max(1, math.floor(main_view.w + 0.5)),
			height = math.max(1, math.floor(main_view.h + 0.5)),
		},
		dest = { x = 0, y = 0, width = output.width, height = output.height },
	})
end

--- Collapse multi-view presets to a single main camera without a blank frame.
local function ensure_single_main_view()
	local output = primary_output()
	if not output then
		return
	end
	if not main_view then
		main_view = { x = 0, y = 0, w = output.width, h = output.height }
	end
	if #active_views == 1 and active_views[1] == "main" then
		return
	end
	-- Keep main on-screen first, then drop extras (never go through zero views).
	push_main_view()
	for _, name in ipairs(active_views) do
		if name ~= "main" then
			pcall(lum.remove_view, output.name, name)
		end
	end
	active_views = { "main" }
end

--- View presets: logo+arrows cycle how the output looks into compositor space.
local function apply_view_preset(preset)
	local output = primary_output()
	if not output then
		return
	end
	local w, h = output.width, output.height
	local half_w = math.floor(w / 2)
	local pip_w, pip_h = math.floor(w * 0.28), math.floor(h * 0.28)
	local name = output.name

	if preset == "full" then
		-- One camera: whole output shows (0,0)–(w,h) of the scene.
		set_views(name, {
			{
				name = "main",
				source = { x = 0, y = 0, width = w, height = h },
				dest = { x = 0, y = 0, width = w, height = h },
			},
		})
	elseif preset == "split" then
		-- Two side-by-side cameras into the left/right halves of the scene.
		main_view = nil
		set_views(name, {
			{
				name = "left",
				source = { x = 0, y = 0, width = half_w, height = h },
				dest = { x = 0, y = 0, width = half_w, height = h },
			},
			{
				name = "right",
				source = { x = half_w, y = 0, width = w - half_w, height = h },
				dest = { x = half_w, y = 0, width = w - half_w, height = h },
			},
		})
	elseif preset == "zoom_left" then
		-- Left half of the scene stretched across the full panel.
		set_views(name, {
			{
				name = "main",
				source = { x = 0, y = 0, width = half_w, height = h },
				dest = { x = 0, y = 0, width = w, height = h },
			},
		})
	elseif preset == "pip" then
		-- Full scene plus a magnified PiP of the top-right corner (qalculate).
		set_views(name, {
			{
				name = "main",
				source = { x = 0, y = 0, width = w, height = h },
				dest = { x = 0, y = 0, width = w, height = h },
			},
			{
				name = "pip",
				source = {
					x = w - math.floor(w * 0.35),
					y = 0,
					width = math.floor(w * 0.35),
					height = math.floor(h * 0.45),
				},
				dest = {
					x = w - pip_w - 24,
					y = h - pip_h - 24,
					width = pip_w,
					height = pip_h,
				},
			},
		})
	end
end

--- logo held: view-move mode — middle-drag pans the main camera; scroll zooms it.
local BTN_MIDDLE = 0x112
local view_move_mode = false
local middle_dragging = false
--- Super_L / Super_R refcount so releasing one key does not end the mode early.
local super_held = 0

local function pan_main_view(dx, dy)
	local output = primary_output()
	if not output or not main_view then
		return
	end
	-- Map screen deltas into source space (grab-the-content: drag right → source moves left).
	local scale_x = main_view.w / output.width
	local scale_y = main_view.h / output.height
	main_view.x = main_view.x - dx * scale_x
	main_view.y = main_view.y - dy * scale_y
	push_main_view()
end

local function zoom_main_view(cursor_x, cursor_y, value)
	local output = primary_output()
	if not output or not main_view or value == 0 then
		return
	end
	-- Scroll up / negative → zoom in (smaller source); scroll down → zoom out.
	local factor = value < 0 and (1 / 1.1) or 1.1
	local min_w = math.max(32, math.floor(output.width * 0.05))
	local min_h = math.max(32, math.floor(output.height * 0.05))

	-- Scene point currently under the cursor (monitor → source).
	local fx = cursor_x / output.width
	local fy = cursor_y / output.height
	local focus_x = main_view.x + fx * main_view.w
	local focus_y = main_view.y + fy * main_view.h

	local new_w = math.max(min_w, main_view.w * factor)
	local new_h = math.max(min_h, main_view.h * factor)
	-- Keep aspect ratio locked to the panel.
	local aspect = output.width / output.height
	if new_w / new_h > aspect then
		new_h = new_w / aspect
	else
		new_w = new_h * aspect
	end

	-- Keep that scene point under the cursor after the scale (Miro-style).
	main_view.w = new_w
	main_view.h = new_h
	main_view.x = focus_x - fx * new_w
	main_view.y = focus_y - fy * new_h
	push_main_view()
end

-- Listen only while logo is held; consume so clients don't see the drag/scroll.
lum.on_cursor_click({
	mods = "logo",
	consume = true,
	callback = function(_x, _y, button, pressed)
		if not view_move_mode or button ~= BTN_MIDDLE then
			return
		end
		middle_dragging = pressed
	end,
})
lum.on_cursor_move({
	mods = "logo",
	consume = true,
	callback = function(_x, _y, dx, dy)
		if view_move_mode and middle_dragging then
			pan_main_view(dx, dy)
		end
	end,
})
lum.on_cursor_scroll({
	mods = "logo",
	consume = true,
	callback = function(x, y, axis, value)
		if view_move_mode and axis == 0 then
			zoom_main_view(x, y, value)
		end
	end,
})

local function enter_view_move_mode()
	super_held = super_held + 1
	if super_held ~= 1 then
		return
	end
	view_move_mode = true
	ensure_single_main_view()
end

local function leave_view_move_mode()
	super_held = math.max(0, super_held - 1)
	if super_held ~= 0 then
		return
	end
	view_move_mode = false
	middle_dragging = false
end

lum.add_zone({
	name = "main",
	x = 0,
	y = 0,
	default = true,
	composition = "free",
	default_width = 800,
	default_height = 600,
})

--- Demo guides: layout boxes under windows, snap lines above. logo+g toggles.
local guides_visible = true

local function apply_demo_guides(w, h)
	lum.clear_guides()
	if not guides_visible then
		return
	end

	local term_x, term_y = 40, 40
	local term_w, term_h = math.floor(w * 0.55), h - 80
	local calc_x, calc_y = math.floor(w * 0.62), 80
	local calc_w, calc_h = math.floor(w * 0.32), math.floor(h * 0.4)

	-- Under windows: region outlines for the demo placements.
	lum.add_guide({
		name = "term",
		kind = "box",
		layer = "below",
		x = term_x,
		y = term_y,
		width = term_w,
		height = term_h,
		color = { r = 80, g = 160, b = 255, a = 180 },
		stroke = 2,
		fill = { r = 80, g = 160, b = 255, a = 28 },
		label = "term",
	})
	lum.add_guide({
		name = "calc",
		kind = "box",
		layer = "below",
		x = calc_x,
		y = calc_y,
		width = calc_w,
		height = calc_h,
		color = { r = 255, g = 140, b = 60, a = 200 },
		stroke = 2,
		fill = { r = 255, g = 140, b = 60, a = 28 },
		label = "calc",
	})

	-- Over windows: margin + midline helpers.
	lum.add_guide({
		name = "left-margin",
		kind = "line",
		layer = "above",
		x1 = 40,
		y1 = 0,
		x2 = 40,
		y2 = h,
		color = { r = 255, g = 80, b = 80, a = 200 },
		stroke = 1,
		label = "x40",
	})
	lum.add_guide({
		name = "midline",
		kind = "line",
		layer = "above",
		x1 = 0,
		y1 = math.floor(h / 2),
		x2 = w,
		y2 = math.floor(h / 2),
		color = { r = 180, g = 255, b = 120, a = 160 },
		stroke = 1,
		label = "mid",
	})
end

-- Call set_xkb before map_key so binding key names resolve against the active layout.
lum.set_xkb({
	layout = "de",
})

--- Hold logo to enter view-move mode; release to leave.
for _, key in ipairs({ "Super_L", "Super_R" }) do
	lum.map_key({
		key = key,
		on = "down",
		consume = false,
		callback = enter_view_move_mode,
	})
	lum.map_key({
		key = key,
		on = "up",
		consume = false,
		callback = leave_view_move_mode,
	})
end

lum.on_startup(function()
	local output = enable_outputs()
	apply_view_preset("full")

	local w, h = output.width, output.height
	-- Place clients in global space; views decide what the panel shows.
	lum.clear_window_rules()
	lum.add_window_rule({
		app_id = "org.wezfurlong.wezterm",
		x = 40,
		y = 40,
		width = math.floor(w * 0.55),
		height = h - 80,
	})
	lum.add_window_rule({
		app_id = "io.github.Qalculate.qalculate-qt",
		x = math.floor(w * 0.62),
		y = 80,
		width = math.floor(w * 0.32),
		height = math.floor(h * 0.4),
	})

	apply_demo_guides(w, h)

	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	lum.spawn({ command = "qalculate-qt" })
end)

--- logo+arrows: switch view presets (cameras into the scene).
lum.map_key({
	key = "Down",
	mods = "logo",
	callback = function()
		apply_view_preset("full")
	end,
})
lum.map_key({
	key = "Left",
	mods = "logo",
	callback = function()
		apply_view_preset("zoom_left")
	end,
})
lum.map_key({
	key = "Right",
	mods = "logo",
	callback = function()
		apply_view_preset("split")
	end,
})
lum.map_key({
	key = "Up",
	mods = "logo",
	callback = function()
		apply_view_preset("pip")
	end,
})

lum.map_key({
	key = "m",
	mods = "logo",
	callback = function()
		lum.set_window({ width = 1200, height = 800 })
	end,
})

--- logo+g: toggle demo guides (boxes below windows, lines above).
lum.map_key({
	key = "g",
	mods = "logo",
	callback = function()
		guides_visible = not guides_visible
		local output = primary_output()
		if output then
			apply_demo_guides(output.width, output.height)
		end
	end,
})

--- logo+n: prompt for a new named view (PiP-sized) via lum.ui.
lum.map_key({
	key = "n",
	mods = "logo",
	callback = function()
		lum.ui({
			title = "New view",
			fields = {
				{
					id = "name",
					type = "text",
					label = "Name",
					placeholder = "pip",
					focus = true,
				},
			},
			actions = {
				{ id = "cancel", label = "Cancel" },
				{ id = "ok", label = "Create", primary = true, submit = true },
			},
			on_submit = function(values, action)
				if action ~= "ok" then
					return
				end
				local name = values.name
				if type(name) ~= "string" or name == "" then
					return
				end
				local output = primary_output()
				if not output then
					return
				end
				local w, h = output.width, output.height
				local pip_w, pip_h = math.floor(w * 0.28), math.floor(h * 0.28)
				lum.add_view(output.name, {
					name = name,
					source = {
						x = w - math.floor(w * 0.35),
						y = 0,
						width = math.floor(w * 0.35),
						height = math.floor(h * 0.45),
					},
					dest = {
						x = w - pip_w - 24,
						y = h - pip_h - 24,
						width = pip_w,
						height = pip_h,
					},
				})
				table.insert(active_views, name)
			end,
		})
	end,
})

--- logo+r: toggle PipeWire stream of the primary output.
local pipewire_stream_id = nil
lum.map_key({
	key = "r",
	mods = "logo",
	callback = function()
		if pipewire_stream_id then
			lum.stop_pipewire_stream(pipewire_stream_id)
			pipewire_stream_id = nil
			return
		end
		local output = primary_output()
		if not output then
			return
		end
		local stream = lum.start_pipewire_stream({
			x = output.x,
			y = output.y,
			width = output.width,
			height = output.height,
		})
		pipewire_stream_id = stream.id
	end,
})
