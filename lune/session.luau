--[[
	Makes a copy from a place file a project built itself (`copy` in studio-sync.json):
	the place with the copy's session id on Workspace, which the plugin sends so the
	daemon maps the Studio to the checkout. Run by `studio-sync open --copy`.

	lune run session.luau <place.rbxl> <out.rbxl> <session>
]]

local fs = require("@lune/fs")
local process = require("@lune/process")
local roblox = require("@lune/roblox")

local source, out, session = table.unpack(process.args)
local game = roblox.deserializePlace(fs.readFile(source))
game:GetService("Workspace"):SetAttribute("StudioSyncSession", session)
fs.writeFile(out, roblox.serializePlace(game))
