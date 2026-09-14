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
	end
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

lum.add_zone({
	name = "main",
	x = 0,
	y = 0,
	default = true,
	composition = "free",
	default_width = 800,
	default_height = 600,
})

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

	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
	lum.spawn({ command = "qalculate-qt" })
end)

-- Call set_xkb before map_key so binding key names resolve against the active layout.
lum.set_xkb({
	layout = "de",
})

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
