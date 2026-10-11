--[[
	Puts a Rojo build (code + snapshot content) onto a base place, so the result
	keeps the base's service settings that Studio won't let a plugin read
	(Workspace.ExplicitAutoJoints, SignalBehavior, streaming, Lighting.Technology,
	...). Run by `studio-sync build`; no project files are read here.

	lune run merge.luau <base.rbxl> <built.rbxl> <out.rbxl> <readable.json>

	readable.json: { [service]: { property names the snapshot captured } }; those
	(and every attribute) come from the build, everything else from the base.
]]

local fs = require("@lune/fs")
local process = require("@lune/process")
local roblox = require("@lune/roblox")
local serde = require("@lune/serde")

local basePath, builtPath, outPath, readablePath = table.unpack(process.args)
local base = roblox.deserializePlace(fs.readFile(basePath))
local built = roblox.deserializePlace(fs.readFile(builtPath))
local readable = serde.decode("json", fs.readFile(readablePath))

-- Instances the engine owns in every place; the base's stay.
local KEEP = { Camera = true }

for _, service in built:GetChildren() do
	local ok, target = pcall(base.GetService, base, service.ClassName)
	if not ok or not target then
		target = base:FindFirstChild(service.Name)
	end
	if not target then
		service.Parent = base
		continue
	end
	for _, child in target:GetChildren() do
		if not KEEP[child.ClassName] then
			child:Destroy()
		end
	end
	for _, child in service:GetChildren() do
		if not KEEP[child.ClassName] then
			child.Parent = target
		end
	end
	for _, property in readable[service.Name] or {} do
		pcall(function()
			(target :: any)[property] = (service :: any)[property]
		end)
	end
	for name in target:GetAttributes() do
		if not name:match("^RBX") then -- engine-reserved; the plugin never sees these
			target:SetAttribute(name, nil)
		end
	end
	for name, value in service:GetAttributes() do
		if not name:match("^RBX") then
			target:SetAttribute(name, value)
		end
	end
end

fs.writeFile(outPath, roblox.serializePlace(base))
