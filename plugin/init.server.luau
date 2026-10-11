-- The studio-sync plugin: syncs this place with the studio-sync daemon while it
-- is open in Edit and a registered project lists its place id. The logic is in Sync.
local RunService = game:GetService("RunService")
local Selection = game:GetService("Selection")

if not RunService:IsEdit() then
	return
end

local Sync = require(script.Sync)

local toolbar = plugin:CreateToolbar("Studio Sync")
local syncButton = toolbar:CreateButton(
	"StudioSync",
	"Studio Sync: lit while syncing; click to pause or resume (resuming makes Studio match disk again)",
	"",
	"Sync"
)
syncButton.ClickableWhenViewportHidden = true
local saveButton = toolbar:CreateButton(
	"StudioSyncSave",
	"Save the selected instances (models, parts, values...) into the repo as .rbxm. Scripts and folders save by themselves.",
	"",
	"Save to disk"
)
saveButton.ClickableWhenViewportHidden = true

local state
state = Sync.start({
	log = print,
	onStatus = function(status)
		syncButton:SetActive(status == "connected")
	end,
})

syncButton.Click:Connect(function()
	Sync.setPaused(state, not state.paused)
	if state.paused then
		print("[studio-sync] paused: Studio edits are not saved and disk changes not applied")
	else
		print("[studio-sync] resumed: syncing Studio to disk again")
		state.status = "offline"
	end
	syncButton:SetActive(false)
end)

saveButton.Click:Connect(function()
	saveButton:SetActive(false)
	if state.status ~= "connected" then
		warn("[studio-sync] not connected; nothing saved")
		return
	end
	local selected = Selection:Get()
	if #selected == 0 then
		warn("[studio-sync] select what to save first")
		return
	end
	for _, instance in selected do
		task.spawn(function()
			local ok, message = Sync.pull(state, instance)
			if ok then
				print(`[studio-sync] saved {instance:GetFullName()} to {message}`)
			else
				warn(`[studio-sync] {instance:GetFullName()} not saved: {message}`)
			end
		end)
	end
end)

plugin.Unloading:Connect(function()
	Sync.stop(state)
end)
