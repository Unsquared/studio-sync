--[[
	Makes a base place: a place file with every service's settings and none of
	its content. `studio-sync base <place.rbxl>` writes it to <snapshot dir>/base.rbxl;
	`studio-sync build` lays code and the snapshot on top of it.

	lune run base.luau <place.rbxl> <out.rbxl>
]]

local fs = require("@lune/fs")
local process = require("@lune/process")
local roblox = require("@lune/roblox")

local source, out = table.unpack(process.args)
local game = roblox.deserializePlace(fs.readFile(source))
local removed = 0
for _, service in game:GetChildren() do
	for _, child in service:GetChildren() do
		if child.ClassName ~= "Camera" then
			removed += #child:GetDescendants() + 1
			child:Destroy()
		end
	end
end
fs.writeFile(out, roblox.serializePlace(game))
print(`base place: {#game:GetChildren()} services, {removed} content instance(s) left out`)
