local lum = require("lumalla")

--- Pick preferred mode, else the first advertised mode.
local function preferred_mode(connector)
	for _, mode in ipairs(connector.modes or {}) do
		if mode.preferred then
			return mode
		end
	end
	return (connector.modes or {})[1]
end

--- Collect connected connectors across all DRM devices.
local function connected_connectors(devices)
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
	return connectors
end

local function enable_all_outputs(devices)
	local connectors = connected_connectors(devices)
	local configs = {}
	local enabled = {}

	for _, connector in ipairs(connectors) do
		local mode = preferred_mode(connector)
		table.insert(configs, {
			name = connector.name,
			enabled = true,
			mode = mode and mode.name or nil,
		})
		enabled[connector.name] = { connector = connector, mode = mode }
	end

	if #configs > 0 then
		lum.set_output_configs(configs)
	end

	-- Drop logical outputs that are no longer connected.
	for _, output in ipairs(lum.get_outputs()) do
		if not output.virtual and not enabled[output.name] then
			lum.remove_output(output.name)
		end
	end

	-- Add / keep logical outputs for every connected connector.
	local existing = {}
	for _, output in ipairs(lum.get_outputs()) do
		existing[output.name] = true
	end

	local x = 0
	for _, connector in ipairs(connectors) do
		local mode = enabled[connector.name].mode
		local width = mode and mode.width or 0
		local height = mode and mode.height or 0
		local refresh_mhz = mode and (mode.refresh_hz * 1000) or 60000

		if not existing[connector.name] and width > 0 and height > 0 then
			lum.add_output({
				name = connector.name,
				description = connector.connector_type .. " " .. connector.name,
				x = x,
				y = 0,
				width = width,
				height = height,
				refresh_mhz = refresh_mhz,
				mm_width = connector.mm_width,
				mm_height = connector.mm_height,
				scale = 1,
				virtual = false,
			})
		end
		x = x + width
	end
end

lum.on_drm_devices_change(function(devices)
	enable_all_outputs(devices)
end)

lum.on_startup(function()
	enable_all_outputs(lum.get_drm_devices())
	lum.spawn({ command = "qalculate-qt" })
	lum.spawn({ command = "wezterm", args = { "start", "--always-new-process" } })
end)

lum.add_window_rule({
	app_id = "io.github.Qalculate.qalculate-qt",
	x = 1400,
	y = 100,
	width = 400,
	height = 600,
})
lum.map_key({
	key = "m",
	mods = "logo",
	callback = function()
		lum.set_window({ width = 1200, height = 800 })
	end,
})
