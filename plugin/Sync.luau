--[[
	Studio side of studio-sync. Asks the daemon (localhost:34999) for the
	project that owns this place id, applies its full tree, then both ways:

	- disk -> Studio: long-polls /changes and applies each delta (creates,
	  deletes, moves, Source, properties, attributes, .rbxm models);
	- Studio -> disk: watches the synced roots and posts edits to /writeback
	  (Source, new scripts and folders, renames, moves, deletes). Disk wins a
	  conflict; Studio's text is saved in the daemon's backups.

	Other instances (Parts, models, values) made in Studio are saved with
	Sync.pull (the toolbar's "Save to disk", or `studio-sync pull` from a shell).

	No dependency on the `plugin` global, so the same source runs from the
	plugin, the command bar or an MCP:

		local Sync = loadstring(game:GetService("HttpService"):GetAsync("http://localhost:34999/plugin/sync.luau"))()
		local s = Sync.new({ writeback = false })
		print(Sync.connect(s))      -- one full sync; Sync.poll(s) applies one delta
]]

local CollectionService = game:GetService("CollectionService")
local EncodingService = game:GetService("EncodingService")
local HttpService = game:GetService("HttpService")
local ScriptEditorService = game:GetService("ScriptEditorService")
local SerializationService = game:GetService("SerializationService")

local PLUGIN_VERSION = "2.1.0"
local DEFAULT_URL = "http://localhost:34999"
local RETRY_OFFLINE = 3
local RETRY_UNKNOWN = 6
local SOURCE_DEBOUNCE = 0.6
local STRUCTURE_DEBOUNCE = 0.4
-- HttpService posts at most 1 MB; parts stay under it
local POST_LIMIT = 900 * 1024
local SEP = "\0"

local Sync = {}

local function keyOf(path: { string }): string
	return table.concat(path, SEP)
end

local function display(path: { string }): string
	return table.concat(path, ".")
end

local function isScript(instance: Instance): boolean
	return instance:IsA("LuaSourceContainer") and instance.ClassName ~= "CoreScript"
end

local function setStatus(s, status: string)
	if s.status == status then
		return
	end
	s.status = status
	if s.onStatus then
		s.onStatus(status)
	end
end

local function warnOnce(s, key: string, message: string)
	if s.warned[key] then
		return
	end
	s.warned[key] = true
	s.log(`[studio-sync] {message}`)
end

function Sync.new(options: { [string]: any })
	return {
		base = options.base or DEFAULT_URL,
		placeId = options.placeId or game.PlaceId,
		-- a copy made by `studio-sync open --copy` names its checkout through this attribute
		session = options.session or workspace:GetAttribute("StudioSyncSession"),
		clientId = HttpService:GenerateGUID(false),
		log = options.log or print,
		onStatus = options.onStatus,
		writebackEnabled = options.writeback ~= false,
		status = "offline",
		paused = false,
		stopped = false,
		name = nil,
		epoch = nil,
		version = 0,
		roots = {},
		writeback = false,
		known = {}, -- key -> { path, className, kind, ro, hash, source, sourceHash, modelHash, instance }
		byInstance = {}, -- Instance -> key
		dirtySource = {}, -- Instance -> os.clock() of the last edit
		structureAt = nil, -- os.clock() of the last structural change, nil when none
		rejected = {}, -- Instance -> true: the daemon refused it; left alone until the next full sync
		adopt = {}, -- key -> true: a pulled instance becomes its model node without being replaced
		hooked = {}, -- Instance -> { RBXScriptConnection }
		connections = {},
		warned = {},
		errors = {},
		unsaved = {},
		reportedUnsaved = "",
	}
end

-- -- HTTP --------------------------------------------------------------------

local function url(s, path: string): string
	local separator = if string.find(path, "?", 1, true) then "&" else "?"
	local session = if s.session then `&session={s.session}` else ""
	return `{s.base}{path}{separator}placeId={s.placeId}{session}&client={s.clientId}&plugin={PLUGIN_VERSION}`
end

-- Returns the status code and body; raises only when the daemon is unreachable.
local function request(s, method: string, path: string, body: string?): (number, string)
	local response = HttpService:RequestAsync({
		Url = url(s, path),
		Method = method,
		Body = body,
		Headers = if body then { ["Content-Type"] = "application/json" } else nil,
	})
	return response.StatusCode, response.Body
end

local function postJson(s, path: string, value: any): any
	local status, body = request(s, "POST", path, HttpService:JSONEncode(value))
	if status ~= 200 and status ~= 422 then
		error(`daemon answered {status}: {body}`)
	end
	return HttpService:JSONDecode(body)
end

-- -- the DataModel -----------------------------------------------------------

local function getService(name: string): Instance?
	local ok, service = pcall(function()
		return game:GetService(name)
	end)
	return if ok then service else nil
end

local function find(path: { string }): Instance?
	local current: Instance? = getService(path[1])
	for index = 2, #path do
		if not current then
			return nil
		end
		current = current:FindFirstChild(path[index])
	end
	return current
end

local function pathOf(instance: Instance): { string }?
	local path = {}
	local current: Instance? = instance
	while current and current ~= game do
		table.insert(path, 1, current.Name)
		current = current.Parent
	end
	return if current == game then path else nil
end

-- The instance at path, made (Folders for missing parents) or replaced when its class differs.
local function resolve(path: { string }, className: string): (Instance?, boolean)
	local current = getService(path[1])
	if not current then
		return nil, false
	end
	local created = false
	for index = 2, #path do
		local name = path[index]
		local isLeaf = index == #path
		local child = current:FindFirstChild(name)
		if child and isLeaf and child.ClassName ~= className then
			local replacement = Instance.new(className)
			replacement.Name = name
			for _, grandchild in child:GetChildren() do
				grandchild.Parent = replacement
			end
			child:Destroy()
			replacement.Parent = current
			child = replacement
			created = true
		elseif not child then
			child = Instance.new(if isLeaf then className else "Folder")
			child.Name = name
			child.Parent = current
			created = true
		end
		current = child
	end
	return current, created
end

-- An open script's text is the editor's (it may be ahead of .Source).
local function currentText(instance: Instance): string
	local ok, document = pcall(function()
		return ScriptEditorService:FindScriptDocument(instance :: any)
	end)
	if ok and document then
		return document:GetText()
	end
	return (instance :: any).Source
end

-- An open script takes its new Source through the editor so the open tab follows.
local function setSource(instance: Instance, source: string): boolean
	if currentText(instance) == source and (instance :: any).Source == source then
		return false
	end
	local ok, document = pcall(function()
		return ScriptEditorService:FindScriptDocument(instance :: any)
	end)
	if ok and document then
		if
			pcall(function()
				ScriptEditorService:UpdateSourceAsync(instance :: any, function()
					return source
				end)
			end)
		then
			return true
		end
	end
	(instance :: any).Source = source
	return true
end

-- -- Rojo property values ------------------------------------------------------

local function enumValue(enumType: any, value: any): EnumItem
	if type(value) == "number" then
		return enumType:FromValue(value)
	end
	return enumType[value]
end

local function cframe(v: any): CFrame
	if v.position then
		local p, o = v.position, v.orientation
		return CFrame.new(
			p[1],
			p[2],
			p[3],
			o[1][1],
			o[2][1],
			o[3][1],
			o[1][2],
			o[2][2],
			o[3][2],
			o[1][3],
			o[2][3],
			o[3][3]
		)
	end
	return CFrame.new(table.unpack(v))
end

local function sequence(kind: string, v: any): any
	local keypoints = {}
	for _, k in v.keypoints do
		if kind == "Color" then
			table.insert(keypoints, ColorSequenceKeypoint.new(k.time, Color3.new(table.unpack(k.color))))
		else
			table.insert(keypoints, NumberSequenceKeypoint.new(k.time, k.value, k.envelope or 0))
		end
	end
	return if kind == "Color" then ColorSequence.new(keypoints) else NumberSequence.new(keypoints)
end

-- A value from Rojo's JSON, as the type `like` (the property's current value) has.
local function decode(like: any, value: any): any
	local kind = typeof(like)
	if type(value) == "table" and not value[1] then
		-- the explicit form: { "Vector3": [1, 2, 3] }
		local tag, inner = next(value)
		if tag == "Vector3" then
			return Vector3.new(table.unpack(inner))
		elseif tag == "Vector2" then
			return Vector2.new(table.unpack(inner))
		elseif tag == "Color3" then
			return Color3.new(table.unpack(inner))
		elseif tag == "Color3uint8" then
			return Color3.fromRGB(table.unpack(inner))
		elseif tag == "UDim" then
			return UDim.new(table.unpack(inner))
		elseif tag == "UDim2" then
			return UDim2.new(inner[1][1], inner[1][2], inner[2][1], inner[2][2])
		elseif tag == "CFrame" then
			return cframe(inner)
		elseif tag == "Enum" or tag == "EnumItem" then
			return enumValue(like.EnumType, if type(inner) == "table" then inner.value else inner)
		elseif tag == "BrickColor" then
			return BrickColor.new(inner)
		elseif tag == "NumberRange" then
			return NumberRange.new(table.unpack(inner))
		elseif tag == "NumberSequence" or tag == "ColorSequence" then
			return sequence(if tag == "ColorSequence" then "Color" else "Number", inner)
		elseif tag == "Font" then
			return Font.new(
				inner.family,
				Enum.FontWeight[inner.weight or "Regular"],
				Enum.FontStyle[inner.style or "Normal"]
			)
		elseif tag == "Content" or tag == "ContentId" then
			return if kind == "Content" then Content.fromUri(inner) else inner
		elseif tag == "Rect" then
			return Rect.new(inner[1][1], inner[1][2], inner[2][1], inner[2][2])
		elseif tag ~= nil then
			return inner -- String, Bool, Float32, Float64, Int32, Int64
		end
	end
	if kind == "Vector3" then
		return Vector3.new(table.unpack(value))
	elseif kind == "Vector2" then
		return Vector2.new(table.unpack(value))
	elseif kind == "Color3" then
		return Color3.new(table.unpack(value))
	elseif kind == "UDim" then
		return UDim.new(table.unpack(value))
	elseif kind == "UDim2" then
		return UDim2.new(value[1][1], value[1][2], value[2][1], value[2][2])
	elseif kind == "CFrame" then
		return cframe(value)
	elseif kind == "EnumItem" then
		return enumValue(like.EnumType, value)
	elseif kind == "BrickColor" then
		return if type(value) == "number" then BrickColor.new(value) else BrickColor.new(value)
	elseif kind == "NumberRange" then
		return NumberRange.new(table.unpack(value))
	elseif kind == "Content" then
		return Content.fromUri(value)
	elseif kind == "Font" then
		return Font.new(
			value.family,
			Enum.FontWeight[value.weight or "Regular"],
			Enum.FontStyle[value.style or "Normal"]
		)
	end
	return value
end

-- An attribute value: explicit Rojo form or plain JSON.
local function decodeAttribute(value: any): any
	if type(value) == "table" and not value[1] and next(value) ~= nil then
		local tag = next(value)
		local probe = {
			Vector3 = Vector3.zero,
			Vector2 = Vector2.zero,
			Color3 = Color3.new(),
			UDim = UDim.new(),
			UDim2 = UDim2.new(),
			CFrame = CFrame.identity,
			BrickColor = BrickColor.new(1),
			NumberRange = NumberRange.new(0),
			Font = Font.new("rbxasset://fonts/families/SourceSansPro.json"),
		}
		return decode(probe[tag], value)
	end
	return value
end

local function setProperties(instance: Instance, properties: { [string]: any }, errors: { string })
	for property, value in properties do
		local ok, err = pcall(function()
			if property == "Tags" then
				for _, tag in (if type(value) == "table" and value.Tags then value.Tags else value) do
					CollectionService:AddTag(instance, tag)
				end
			elseif property == "Attributes" then
				for name, attribute in (if value.Attributes then value.Attributes else value) do
					instance:SetAttribute(name, decodeAttribute(attribute))
				end
			else
				local like = (instance :: any)[property];
				(instance :: any)[property] = decode(like, value)
			end
		end)
		if not ok then
			table.insert(errors, `{instance:GetFullName()}.{property}: {err}`)
		end
	end
end

-- -- known: what the plugin last saw on disk -----------------------------------

local function remember(s, node: any, instance: Instance)
	local key = keyOf(node.path)
	local previous = s.known[key]
	if previous and previous.instance ~= instance then
		s.byInstance[previous.instance] = nil
	end
	s.known[key] = {
		path = node.path,
		className = node.className,
		kind = node.kind,
		ro = node.ro == true,
		hash = node.hash,
		source = node.source,
		sourceHash = node.sourceHash,
		modelHash = node.modelHash,
		instance = instance,
	}
	s.byInstance[instance] = key
end

local function forget(s, path: { string })
	local prefix = keyOf(path)
	for key, entry in s.known do
		if key == prefix or string.sub(key, 1, #prefix + 1) == prefix .. SEP then
			s.known[key] = nil
			if s.byInstance[entry.instance] == key then
				s.byInstance[entry.instance] = nil
			end
		end
	end
end

local function rekey(s, from: { string }, to: { string })
	local prefix = keyOf(from)
	local moved = {}
	for key, entry in s.known do
		if key == prefix or string.sub(key, 1, #prefix + 1) == prefix .. SEP then
			s.known[key] = nil
			local path = table.clone(to)
			for index = #from + 1, #entry.path do
				table.insert(path, entry.path[index])
			end
			entry.path = path
			table.insert(moved, entry)
		end
	end
	for _, entry in moved do
		local key = keyOf(entry.path)
		s.known[key] = entry
		s.byInstance[entry.instance] = key
	end
end

local function structural(entry: any): boolean
	return not entry.ro and (entry.kind == "script" or entry.kind == "init" or entry.kind == "dir")
end

-- -- disk -> Studio ------------------------------------------------------------

type Counts = { created: number, updated: number, removed: number }

local function loadModel(s, node: any): Instance
	local status, body = request(s, "GET", `/model?file={HttpService:UrlEncode(node.model)}`)
	if status ~= 200 then
		error(`{node.model}: daemon answered {status}: {body}`)
	end
	local blob = EncodingService:Base64Decode(buffer.fromstring(body))
	local instances = SerializationService:DeserializeInstancesAsync(blob)
	assert(#instances == 1, `{node.model} holds {#instances} root instances; a model file needs exactly one`)
	return instances[1]
end

local function applyModel(s, node: any, counts: Counts): Instance?
	local key = keyOf(node.path)
	local existing = find(node.path)
	local previous = s.known[key]
	if
		existing
		and (s.adopt[key] or (previous and previous.instance == existing and previous.modelHash == node.modelHash))
	then
		s.adopt[key] = nil
		return existing
	end
	local parentPath = table.clone(node.path)
	table.remove(parentPath)
	local parent = if #parentPath == 1 then getService(parentPath[1]) else resolve(parentPath, "Folder")
	assert(parent, `no parent for {display(node.path)}`)
	local loaded = loadModel(s, node)
	loaded.Name = node.path[#node.path]
	if existing then
		existing:Destroy()
		counts.removed += 1
	end
	loaded.Parent = parent
	counts.created += 1
	return loaded
end

-- Returns the instance and the node to remember (with the old base when Studio's edit is pending).
local function applyNode(s, node: any, counts: Counts, errors: { string }): (Instance?, any)
	if node.error then
		table.insert(errors, node.error)
	end
	if node.kind == "model" then
		return applyModel(s, node, counts), node
	end
	local instance, created = resolve(node.path, node.className)
	if not instance then
		table.insert(errors, `{display(node.path)}: no service {node.path[1]}`)
		return nil, node
	end
	if created then
		counts.created += 1
	end
	local key = keyOf(node.path)
	local previous = s.known[key]
	if node.source and isScript(instance) then
		if s.dirtySource[instance] and currentText(instance) ~= node.source and previous then
			-- edited in Studio and on disk: keep the old base so the writeback reports a conflict
			node = table.clone(node)
			node.hash = nil
			node.source = previous.source
			node.sourceHash = previous.sourceHash
		elseif setSource(instance, node.source) then
			counts.updated += 1
		end
	end
	if node.runContext and (instance :: any).RunContext ~= Enum.RunContext[node.runContext] then
		(instance :: any).RunContext = Enum.RunContext[node.runContext]
	end
	if node.properties then
		setProperties(instance, node.properties, errors)
	end
	if node.attributes then
		for name, value in node.attributes do
			local ok, err = pcall(instance.SetAttribute, instance, name, decodeAttribute(value))
			if not ok then
				table.insert(errors, `{instance:GetFullName()} attribute {name}: {err}`)
			end
		end
	end
	return instance, node
end

local function byDepth(a: any, b: any): boolean
	return #a.path < #b.path
end

local function report(s, errors: { string })
	local unsaved = table.concat(s.unsaved, "\n")
	if #errors == 0 and #s.errors == 0 and unsaved == s.reportedUnsaved then
		return
	end
	s.errors = errors
	s.reportedUnsaved = unsaved
	for _, err in errors do
		s.log(`[studio-sync] {err}`)
	end
	pcall(postJson, s, "/report", { version = s.version, errors = errors, unsaved = s.unsaved })
end

local function summary(counts: Counts): string
	return `created {counts.created}, updated {counts.updated}, removed {counts.removed}`
end

-- -- Studio -> disk: watching ----------------------------------------------------

local function inRoots(s, instance: Instance): boolean
	for _, root in s.rootInstances or {} do
		if instance == root or instance:IsDescendantOf(root) then
			return true
		end
	end
	return false
end

local function markStructure(s)
	s.structureAt = os.clock()
end

local function hook(s, instance: Instance)
	if s.hooked[instance] or not (isScript(instance) or instance:IsA("Folder")) then
		return
	end
	local list = {
		instance:GetPropertyChangedSignal("Name"):Connect(function()
			markStructure(s)
		end),
	}
	if isScript(instance) then
		table.insert(
			list,
			instance:GetPropertyChangedSignal("Source"):Connect(function()
				s.dirtySource[instance] = os.clock()
			end)
		)
	end
	s.hooked[instance] = list
end

local function unhookAll(s)
	for _, list in s.hooked do
		for _, connection in list do
			connection:Disconnect()
		end
	end
	for _, connection in s.connections do
		connection:Disconnect()
	end
	s.hooked, s.connections = {}, {}
end

local function watch(s)
	unhookAll(s)
	s.rootInstances = {}
	for _, root in s.roots do
		local instance = find(root)
		if instance then
			table.insert(s.rootInstances, instance)
			hook(s, instance)
			for _, descendant in instance:GetDescendants() do
				hook(s, descendant)
			end
			table.insert(
				s.connections,
				instance.DescendantAdded:Connect(function(descendant)
					hook(s, descendant)
					markStructure(s)
				end)
			)
			table.insert(
				s.connections,
				instance.DescendantRemoving:Connect(function()
					markStructure(s)
				end)
			)
		end
	end
	table.insert(
		s.connections,
		ScriptEditorService.TextDocumentDidChange:Connect(function(document)
			local ok, script = pcall(function()
				return document:GetScript()
			end)
			if ok and script and inRoots(s, script) then
				s.dirtySource[script] = os.clock()
			end
		end)
	)
end

-- -- full sync ---------------------------------------------------------------------

local function serializeBase64(instance: Instance): string
	local blob = SerializationService:SerializeInstancesAsync({ instance })
	return buffer.tostring(EncodingService:Base64Encode(blob))
end

-- Posts the Studio side of what a full sync replaces, in parts under the post limit.
local function backup(s, items: { any }): string?
	local batch, size, dir = {}, 0, nil
	local function flush()
		if #batch > 0 then
			dir = postJson(s, "/backup", { items = batch }).dir
			batch, size = {}, 0
		end
	end
	for _, item in items do
		local itemSize = #(item.source or item.rbxm or "") + 200
		if size + itemSize > POST_LIMIT then
			flush()
		end
		table.insert(batch, item)
		size += itemSize
	end
	flush()
	return dir
end

local function applyFull(s, data: any): (Counts, { string })
	local counts = { created = 0, updated = 0, removed = 0 }
	local errors = {}
	local nodes = data.nodes
	table.sort(nodes, byDepth)

	-- Studio text that disk is about to replace is backed up first
	local items = {}
	for _, node in nodes do
		if node.source then
			local existing = find(node.path)
			if existing and isScript(existing) then
				local text = currentText(existing)
				if text ~= node.source and text ~= "" then
					table.insert(items, { path = node.path, source = text })
				end
			end
		end
	end

	s.known, s.byInstance, s.dirtySource, s.rejected, s.structureAt = {}, {}, {}, {}, nil
	local wanted: { [Instance]: boolean } = {}
	for _, node in nodes do
		local ok, result, effective = pcall(applyNode, s, node, counts, errors)
		if not ok then
			table.insert(errors, `{display(node.path)}: {result}`)
		elseif result then
			wanted[result] = true
			remember(s, effective, result)
			if node.kind == "model" then
				for _, descendant in result:GetDescendants() do
					wanted[descendant] = true
				end
			end
		end
	end

	-- what is under the roots but not on disk: backed up, then removed
	local doomed = {}
	for _, root in data.roots do
		local top = find(root)
		if top then
			for _, descendant in top:GetDescendants() do
				-- only the top of each unwanted subtree
				if not wanted[descendant] and descendant.Parent and wanted[descendant.Parent] then
					table.insert(doomed, descendant)
				end
			end
		end
	end
	for _, instance in doomed do
		local path = pathOf(instance)
		if isScript(instance) and #instance:GetChildren() == 0 then
			table.insert(items, { path = path, source = currentText(instance) })
		else
			local ok, encoded = pcall(serializeBase64, instance)
			if ok and #encoded < POST_LIMIT then
				table.insert(items, { path = path, rbxm = encoded })
			else
				table.insert(errors, `{instance:GetFullName()} is not on disk and too big to back up; left in place`)
				wanted[instance] = true
			end
		end
	end
	if #items > 0 then
		local dir = backup(s, items)
		s.log(`[studio-sync] {#items} Studio item(s) differed from disk; Studio's copies saved in {dir}`)
	end
	for _, instance in doomed do
		if not wanted[instance] and instance.Parent then
			instance:Destroy()
			counts.removed += 1
		end
	end

	s.epoch, s.version, s.roots, s.name = data.epoch, data.version, data.roots, data.name
	s.writeback = data.writeback and s.writebackEnabled
	watch(s)
	return counts, errors
end

local function applyDelta(s, data: any): (Counts, { string })
	local counts = { created = 0, updated = 0, removed = 0 }
	local errors = {}
	for _, path in data.removes do
		local entry = s.known[keyOf(path)]
		local instance = if entry and entry.instance.Parent then entry.instance else find(path)
		if instance and instance.Parent and pathOf(instance) and keyOf(pathOf(instance) :: any) == keyOf(path) then
			instance:Destroy()
			counts.removed += 1
		end
		forget(s, path)
	end
	table.sort(data.upserts, byDepth)
	for _, node in data.upserts do
		local ok, result, effective = pcall(applyNode, s, node, counts, errors)
		if not ok then
			table.insert(errors, `{display(node.path)}: {result}`)
		elseif result then
			remember(s, effective, result)
			hook(s, result)
		end
	end
	s.version = data.version
	if data.writeback ~= nil then
		s.writeback = data.writeback and s.writebackEnabled
	end
	return counts, errors
end

-- -- Studio -> disk: writeback -------------------------------------------------------

-- Compares the roots with `known`: new scripts and folders, renames and moves, deletions.
local function scan(s): ({ any }, { any })
	local ops, effects = {}, {}
	local seen: { [Instance]: boolean } = {}
	local unsaved = {}

	local function visit(instance: Instance, parentPath: { string }?, moved: boolean, opaque: boolean)
		local path = if parentPath then table.clone(parentPath) else pathOf(instance) :: { string }
		if parentPath then
			table.insert(path, instance.Name)
		end
		local key = keyOf(path)
		local oldKey = s.byInstance[instance]
		local entry = oldKey and s.known[oldKey]
		if entry then
			seen[instance] = true
			if oldKey ~= key and not moved then
				if structural(entry) and not s.rejected[instance] then
					table.insert(ops, { op = "move", from = entry.path, to = path })
					table.insert(effects, { kind = "move", from = entry.path, to = path, instance = instance })
					moved = true
				else
					warnOnce(
						s,
						"move" .. key,
						`{display(path)} was renamed or moved in Studio only ({entry.kind} content: change its file)`
					)
				end
			end
			opaque = entry.kind == "model" or entry.kind == "json" or entry.kind == "generated"
		elseif opaque or s.rejected[instance] then
			return
		elseif isScript(instance) or instance.ClassName == "Folder" then
			local op = { op = "create", path = path, className = instance.ClassName }
			if isScript(instance) then
				op.source = currentText(instance)
				op.runContext = if instance:IsA("Script") then (instance :: any).RunContext.Name else nil
			end
			table.insert(ops, op)
			table.insert(effects, { kind = "create", path = path, instance = instance, op = op })
			moved = true -- its children are new too, not moved
		else
			table.insert(unsaved, display(path))
			return
		end
		for _, child in instance:GetChildren() do
			visit(child, path, moved, opaque)
		end
	end

	for _, root in s.rootInstances do
		if root.Parent then
			visit(root, nil, false, false)
		end
	end

	-- known entries no longer under the roots were deleted (or moved out)
	local deleted = {}
	for key, entry in s.known do
		if not seen[entry.instance] and not entry.gone then
			if structural(entry) then
				deleted[key] = entry
			else
				entry.gone = true
				local parentPath = table.clone(entry.path)
				table.remove(parentPath)
				local parent = s.known[keyOf(parentPath)]
				if parent and not seen[parent.instance] then
					continue -- its parent's removal covers it
				end
				s.log(
					`[studio-sync] {display(entry.path)} was removed in Studio only ({entry.kind} content: delete its file to remove it)`
				)
			end
		end
	end
	for key, entry in deleted do
		local parentPath = table.clone(entry.path)
		table.remove(parentPath)
		if not deleted[keyOf(parentPath)] then
			table.insert(ops, 1, { op = "delete", path = entry.path })
			table.insert(effects, 1, { kind = "delete", path = entry.path, entry = entry })
		end
	end
	s.unsaved = unsaved
	return ops, effects
end

local function sourceOps(s, ops: { any }, effects: { any }, force: boolean?)
	local now = os.clock()
	for instance, at in s.dirtySource do
		if not force and now - at < SOURCE_DEBOUNCE then
			continue
		end
		local key = s.byInstance[instance]
		local entry = key and s.known[key]
		local path = pathOf(instance)
		if not entry or not path then
			s.dirtySource[instance] = nil -- new or gone: the scan handles it
			continue
		end
		if keyOf(path) ~= key then
			if not s.structureAt then
				s.dirtySource[instance] = nil
			end
			continue -- wait for the move first
		end
		s.dirtySource[instance] = nil
		local text = currentText(instance)
		if text == entry.source then
			continue
		end
		if entry.ro or (entry.kind ~= "script" and entry.kind ~= "init") then
			warnOnce(s, "ro" .. key, `{display(entry.path)} is read-only here ({entry.kind}); edit its file instead`)
			continue
		end
		table.insert(ops, { op = "source", path = entry.path, source = text, base = entry.sourceHash })
		table.insert(effects, { kind = "source", entry = entry, instance = instance, text = text })
	end
end

local function settle(s, effect: any, result: any)
	if effect.kind == "source" then
		if result.ok then
			effect.entry.source = effect.text
			effect.entry.sourceHash = result.sourceHash
		elseif result.conflict then
			effect.entry.source = result.conflict.source
			effect.entry.sourceHash = result.conflict.sourceHash
			setSource(effect.instance, result.conflict.source)
			s.log(
				`[studio-sync] CONFLICT {display(effect.entry.path)}: changed on disk and in Studio. Kept disk; Studio's text saved to {result.conflict.saved}`
			)
		else
			s.log(`[studio-sync] {display(effect.entry.path)} not saved: {result.error}`)
		end
	elseif effect.kind == "create" then
		if result.ok then
			remember(s, {
				path = effect.path,
				className = effect.op.className,
				kind = if effect.op.className == "Folder" then "dir" else "script",
				source = effect.op.source,
				sourceHash = result.sourceHash,
			}, effect.instance)
			s.log(`[studio-sync] saved new {effect.op.className} {display(effect.path)}`)
		else
			s.rejected[effect.instance] = true
			s.log(`[studio-sync] {display(effect.path)} stays in Studio only: {result.error}`)
		end
	elseif effect.kind == "move" then
		if result.ok then
			rekey(s, effect.from, effect.to)
			s.log(`[studio-sync] moved {display(effect.from)} -> {display(effect.to)}`)
		else
			s.rejected[effect.instance] = true
			s.log(`[studio-sync] {display(effect.to)}: rename not saved: {result.error}`)
		end
	elseif effect.kind == "delete" then
		if result.ok then
			forget(s, effect.path)
			s.log(`[studio-sync] deleted {display(effect.path)} (the daemon keeps a copy)`)
		else
			effect.entry.gone = true
			s.log(`[studio-sync] {display(effect.path)} deleted in Studio only: {result.error}`)
		end
	end
end

local flushNow

-- Sends what changed in Studio. Returns how many edits were sent.
function Sync.flush(s, force: boolean?): number
	if not s.writeback or s.status ~= "connected" then
		return 0
	end
	-- one flush at a time: a second one would resend edits still in flight
	while s.flushing do
		if not force then
			return 0
		end
		task.wait()
	end
	s.flushing = true
	local ok, result = pcall(flushNow, s, force)
	s.flushing = false
	if not ok then
		error(result, 0)
	end
	return result
end

function flushNow(s, force: boolean?): number
	local ops, effects = {}, {}
	if s.structureAt and (force or os.clock() - s.structureAt >= STRUCTURE_DEBOUNCE) then
		s.structureAt = nil
		ops, effects = scan(s)
		report(s, {})
	end
	sourceOps(s, ops, effects, force)
	local start = 1
	while start <= #ops do
		-- batches under the post limit, in order
		local batch, batchEffects, size = {}, {}, 0
		local index = start
		while index <= #ops do
			local opSize = #(ops[index].source or "") + 300
			if #batch > 0 and size + opSize > POST_LIMIT then
				break
			end
			table.insert(batch, ops[index])
			table.insert(batchEffects, effects[index])
			size += opSize
			index += 1
		end
		if size > POST_LIMIT then
			s.log(`[studio-sync] {display(batch[1].path or batch[1].to)} is over 900 KB; not saved (edit it on disk)`)
		else
			local response = postJson(s, "/writeback", { ops = batch })
			for i, result in response.results do
				settle(s, batchEffects[i], result)
			end
		end
		start = index
	end
	return #ops
end

-- -- pull: save any instance as a model file -------------------------------------------

-- Saves `instance` into the repo as .rbxm (next to its parent's files, or at `file`).
function Sync.pull(s, instance: Instance, file: string?, id: string?): (boolean, string)
	local path = pathOf(instance)
	if not path then
		return false, "not in the DataModel"
	end
	local ok, encoded = pcall(serializeBase64, instance)
	if not ok then
		if id then
			pcall(request, s, "POST", `/pull?id={id}&error={HttpService:UrlEncode(tostring(encoded))}`, "")
		end
		return false, tostring(encoded)
	end
	local upload = HttpService:GenerateGUID(false)
	local query =
		`/pull?upload={upload}&id={id or ""}&path={HttpService:UrlEncode(HttpService:JSONEncode(path))}&file={HttpService:UrlEncode(
			file or ""
		)}`
	local parts = math.max(1, math.ceil(#encoded / POST_LIMIT))
	local status, body
	for part = 1, parts do
		local chunk = string.sub(encoded, (part - 1) * POST_LIMIT + 1, part * POST_LIMIT)
		status, body = request(s, "POST", `{query}&part={part}&last={if part == parts then 1 else 0}`, chunk)
		if status ~= 200 and part < parts then
			return false, body
		end
	end
	local result = HttpService:JSONDecode(body)
	if result.ok then
		s.adopt[keyOf(path)] = true
		s.rejected[instance] = nil
		return true, result.file
	end
	return false, result.error
end

-- -- snapshot: the whole place's non-code content, for builds from the repo ------------

local ReflectionService = game:GetService("ReflectionService")

-- Services whose children never belong in a place file.
local SNAPSHOT_SKIP = { CoreGui = true, Selection = true, PluginGuiService = true }
-- Property types that a place file carries but Rojo can't express, or that Studio sets itself
local UNSAVED_TYPES =
	{ RefType = true, SecurityCapabilities = true, SharedString = true, OptionalCoordinateFrame = true }
-- Instances Studio makes for the session, never part of the place
local SESSION_CLASSES = { Player = true, Camera = true }
-- Attributes studio-sync itself puts on the place
local OWN_ATTRIBUTES = { StudioSyncSession = true }

-- A property value in Rojo's explicit form, or nil when Rojo can't express it.
local function encodeValue(engineType: string, value: any): any
	local kind = typeof(value)
	if kind == "boolean" then
		return { Bool = value }
	elseif kind == "number" then
		if engineType == "float" then
			return { Float32 = value }
		elseif engineType == "int" then
			return { Int32 = value }
		elseif engineType == "int64" then
			return { Int64 = value }
		end
		return { Float64 = value }
	elseif kind == "string" then
		return { String = value }
	elseif kind == "Color3" then
		return { Color3 = { value.R, value.G, value.B } }
	elseif kind == "Vector3" then
		return { Vector3 = { value.X, value.Y, value.Z } }
	elseif kind == "Vector2" then
		return { Vector2 = { value.X, value.Y } }
	elseif kind == "UDim" then
		return { UDim = { value.Scale, value.Offset } }
	elseif kind == "UDim2" then
		return { UDim2 = { { value.X.Scale, value.X.Offset }, { value.Y.Scale, value.Y.Offset } } }
	elseif kind == "CFrame" then
		local x, y, z, r00, r01, r02, r10, r11, r12, r20, r21, r22 = value:GetComponents()
		return {
			CFrame = {
				position = { x, y, z },
				orientation = { { r00, r10, r20 }, { r01, r11, r21 }, { r02, r12, r22 } },
			},
		}
	elseif kind == "EnumItem" then
		return { Enum = value.Value }
	elseif kind == "BrickColor" then
		return { BrickColor = value.Number }
	elseif kind == "NumberRange" then
		return { NumberRange = { value.Min, value.Max } }
	elseif kind == "Rect" then
		return { Rect = { { value.Min.X, value.Min.Y }, { value.Max.X, value.Max.Y } } }
	elseif kind == "NumberSequence" then
		local keypoints = {}
		for _, k in value.Keypoints do
			table.insert(keypoints, { time = k.Time, value = k.Value, envelope = k.Envelope })
		end
		return { NumberSequence = { keypoints = keypoints } }
	elseif kind == "ColorSequence" then
		local keypoints = {}
		for _, k in value.Keypoints do
			table.insert(keypoints, { time = k.Time, color = { k.Value.R, k.Value.G, k.Value.B } })
		end
		return { ColorSequence = { keypoints = keypoints } }
	elseif kind == "Font" then
		return { Font = { family = value.Family, weight = value.Weight.Name, style = value.Style.Name } }
	end
	return nil
end

-- Every saved, readable property of a service, and its attributes.
local function serviceSettings(service: Instance, skipped: { string }): (any, any)
	local properties = {}
	local count = 0
	for _, property in ReflectionService:GetPropertiesOfClass(service.ClassName) do
		if not property.Serialized or property.Name == "Name" or property.Name == "Parent" then
			continue
		end
		local ok, value = pcall(function()
			return (service :: any)[property.Name]
		end)
		if not ok then
			continue
		end
		local encoded = encodeValue(property.Type.EngineType, value)
		if encoded then
			properties[property.Name] = encoded
			count += 1
		elseif value ~= nil and not UNSAVED_TYPES[property.Type.EngineType] then
			table.insert(skipped, `{service.Name}.{property.Name} ({property.Type.EngineType})`)
		end
	end
	local attributes = {}
	local attributeCount = 0
	for name, value in service:GetAttributes() do
		if not OWN_ATTRIBUTES[name] then
			local encoded = encodeValue(if type(value) == "number" then "double" else "", value)
			if encoded then
				attributes[name] = encoded
				attributeCount += 1
			else
				table.insert(skipped, `{service.Name} attribute {name} ({typeof(value)})`)
			end
		end
	end
	return if count > 0 then properties else nil, if attributeCount > 0 then attributes else nil
end

local function isSerializedService(service: Instance): boolean
	if SNAPSHOT_SKIP[service.ClassName] then
		return false
	end
	local ok, class = pcall(ReflectionService.GetClass, ReflectionService, service.ClassName)
	return ok and class ~= nil and class.Serialized == true
end

local function upload(s, route: string, encoded: string): (number, string)
	local parts = math.max(1, math.ceil(#encoded / POST_LIMIT))
	local status, body = 0, ""
	for part = 1, parts do
		local chunk = string.sub(encoded, (part - 1) * POST_LIMIT + 1, part * POST_LIMIT)
		status, body = request(s, "POST", `{route}&part={part}&last={if part == parts then 1 else 0}`, chunk)
		if status ~= 200 then
			return status, body
		end
	end
	return status, body
end

-- Saves everything in the place that the project's files don't already own: each
-- service's children as .rbxm, and its saved properties and attributes. The daemon
-- writes them to the project's snapshot dir; `studio-sync build` puts them back.
function Sync.snapshot(s, id: string?): (boolean, string)
	local status, body = request(s, "GET", "/snapshot/plan")
	if status ~= 200 then
		return false, body
	end
	local plan = HttpService:JSONDecode(body)
	local ignored = {}
	for _, path in plan.ignore do
		ignored[keyOf(path)] = true
	end
	local token = HttpService:GenerateGUID(false)
	local entries, services, skipped, duplicates = {}, {}, {}, {}
	local bytes = 0

	local function save(instance: Instance, path: { string })
		local ok, encoded = pcall(serializeBase64, instance)
		if not ok then
			table.insert(skipped, `{display(path)} ({encoded})`)
			return
		end
		local route = `/snapshot/file?upload={token}&path={HttpService:UrlEncode(HttpService:JSONEncode(path))}`
		local code, answer = upload(s, route, encoded)
		if code ~= 200 then
			error(`{display(path)}: daemon answered {code}: {answer}`)
		end
		bytes += #encoded * 3 // 4
		table.insert(entries, { path = path, className = instance.ClassName })
	end

	local function walk(container: Instance, node: any, path: { string })
		local seen = {}
		for _, child in container:GetChildren() do
			local childPath = table.clone(path)
			table.insert(childPath, child.Name)
			if not child.Archivable or ignored[keyOf(childPath)] or SESSION_CLASSES[child.ClassName] then
				continue
			end
			local declared = node and node.children and node.children[child.Name]
			if declared then
				if not declared.owned then
					walk(child, declared, childPath)
				end
			elseif seen[child.Name] then
				table.insert(duplicates, display(childPath))
			else
				seen[child.Name] = true
				save(child, childPath)
			end
		end
	end

	for _, service in game:GetChildren() do
		if not isSerializedService(service) then
			continue
		end
		local node = plan.tree[service.Name]
		if node and node.owned then
			continue
		end
		local properties, attributes = serviceSettings(service, skipped)
		local before = #entries
		walk(service, node, { service.Name })
		if properties or attributes or #entries > before then
			services[service.Name] = { className = service.ClassName, properties = properties, attributes = attributes }
		end
		task.wait() -- keep Studio responsive between services
	end

	local result = postJson(s, `/snapshot/commit?upload={token}&id={id or ""}`, {
		placeId = game.PlaceId,
		entries = entries,
		services = services,
		skipped = skipped,
		duplicates = duplicates,
	})
	if not result.ok then
		return false, result.error
	end
	return true, `{#entries} instance(s), {string.format("%.1f", bytes / 1e6)} MB -> {result.dir}`
end

local function fail(s, id: string, message: string)
	pcall(request, s, "POST", `/command/result?id={id}&error={HttpService:UrlEncode(message)}`, "")
end

local function runCommands(s, commands: { any }?)
	for _, command in commands or {} do
		if command.kind == "snapshot" then
			task.spawn(function()
				local ok, saved, message = pcall(Sync.snapshot, s, command.id)
				if ok and saved then
					s.log(`[studio-sync] snapshot: {message}`)
				else
					local reason = tostring(if ok then message else saved)
					fail(s, command.id, reason)
					s.log(`[studio-sync] snapshot failed: {reason}`)
				end
			end)
		elseif command.kind == "pull" then
			task.spawn(function()
				local instance = find(command.path)
				if not instance then
					pcall(
						request,
						s,
						"POST",
						`/pull?id={command.id}&error={HttpService:UrlEncode("no instance at " .. display(command.path))}`,
						""
					)
					return
				end
				local ok, message = Sync.pull(s, instance, command.file, command.id)
				s.log(`[studio-sync] pull {display(command.path)}: {message}`)
				if not ok then
					pcall(request, s, "POST", `/pull?id={command.id}&error={HttpService:UrlEncode(message)}`, "")
				end
			end)
		end
	end
end

-- -- the loop ----------------------------------------------------------------------

-- One full sync. Returns a summary line, or nil with the reason ("offline" | "unknown place" | a message).
function Sync.connect(s): (string?, string?)
	local ok, status, body = pcall(request, s, "GET", "/tree")
	if not ok then
		return nil, "offline"
	end
	if status == 404 then
		return nil, "unknown place"
	end
	if status == 503 then
		return nil, "offline" -- the daemon is still generating the project
	end
	if status ~= 200 then
		return nil, `daemon answered {status}: {body}`
	end
	local counts, errors = applyFull(s, HttpService:JSONDecode(body))
	report(s, errors)
	return summary(counts)
end

-- Waits for (and applies) the next change; returns its summary, "" when none came,
-- or nil when a full resync is needed.
function Sync.poll(s, wait: number?): string?
	local status, body = request(s, "GET", `/changes?epoch={s.epoch}&since={s.version}&wait={wait or 20}`)
	if status ~= 200 then
		error(`daemon answered {status}: {body}`)
	end
	if s.paused or s.stopped then
		return "" -- resuming does a full sync
	end
	local data = HttpService:JSONDecode(body)
	runCommands(s, data.commands)
	if data.full then
		return nil
	end
	if data.version == s.version then
		return ""
	end
	-- local edits go first, so a delta can't overwrite text that was never saved
	Sync.flush(s, true)
	local counts, errors = applyDelta(s, data)
	report(s, errors)
	return summary(counts)
end

function Sync.setPaused(s, paused: boolean)
	s.paused = paused
	if paused then
		setStatus(s, "paused")
	end
end

function Sync.stop(s)
	s.stopped = true
	unhookAll(s)
end

local function cycle(s)
	if s.status ~= "connected" then
		local line, reason = Sync.connect(s)
		if not line then
			if reason ~= "offline" and reason ~= "unknown place" then
				warnOnce(s, "connect" .. tostring(reason), tostring(reason))
			end
			setStatus(s, if reason == "unknown place" then "unknown" else "offline")
			task.wait(if reason == "offline" then RETRY_OFFLINE else RETRY_UNKNOWN)
			return
		end
		setStatus(s, "connected")
		local mode = if s.writeback then "two-way" else "disk -> Studio only"
		s.log(`[studio-sync] connected: {s.name} v{s.version}, {mode} ({line})`)
		return
	end
	local line = Sync.poll(s)
	if line == nil then
		local resync = Sync.connect(s)
		if resync then
			s.log(`[studio-sync] resynced v{s.version} ({resync})`)
		end
	elseif line ~= "" then
		s.log(`[studio-sync] v{s.version}: {line}`)
	end
end

-- The blocking loops: call from task.spawn. Quiet while the daemon is down.
function Sync.run(s)
	task.spawn(function()
		while not s.stopped do
			task.wait(0.25)
			if not s.paused and s.status == "connected" then
				local ok, err = pcall(Sync.flush, s)
				if not ok then
					warnOnce(s, "flush" .. tostring(err), `writeback failed: {err}`)
				end
			end
		end
	end)
	while not s.stopped do
		if s.paused then
			task.wait(0.5)
		else
			local ok, err = pcall(cycle, s)
			if not ok and not s.stopped and not s.paused then
				if s.status == "connected" then
					s.log(`[studio-sync] disconnected ({err})`)
				end
				setStatus(s, "offline")
				task.wait(RETRY_OFFLINE)
			end
		end
	end
end

function Sync.start(options)
	local s = Sync.new(options)
	task.spawn(Sync.run, s)
	return s
end

Sync.VERSION = PLUGIN_VERSION

return Sync
