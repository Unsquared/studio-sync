--[[
	Runs plugin/Sync.luau in a real Studio against a fake daemon: no HTTP, so it works where the MCP
	may not reach localhost. Instances, serialization and the script editor are the real ones. The
	socket and the daemon's answers are not: tests/daemon.rs tests the daemon, and only a real run
	(CLAUDE.md) tests the two together.

	cd tests && cargo run -- build -o <tmp>/harness-copy-abc123.rbxl, open that in Studio, and
	run in its Edit context (MCP execute_luau):

		return loadstring(game.ServerStorage.SyncHarness.Source)()
]]

local CollectionService = game:GetService("CollectionService")
local EncodingService = game:GetService("EncodingService")
local RealHttp = game:GetService("HttpService")
local SerializationService = game:GetService("SerializationService")
local ServerStorage = game:GetService("ServerStorage")

local ROOT = "StudioSyncTest"
local failures, passed = {}, 0

local function check(name: string, ok: any, detail: any?)
	if ok then
		passed += 1
	else
		table.insert(failures, `{name}{if detail ~= nil then ": " .. tostring(detail) else ""}`)
	end
end

local function untilTrue(condition: () -> any, seconds: number?): any
	local deadline = os.clock() + (seconds or 5)
	while os.clock() < deadline do
		local value = condition()
		if value then
			return value
		end
		task.wait()
	end
	return nil
end

local function newSignal()
	local handlers = {}
	return {
		Connect = function(_, handler)
			table.insert(handlers, handler)
			return {
				Disconnect = function()
					table.remove(handlers, table.find(handlers, handler))
				end,
			}
		end,
		fire = function(_, ...)
			for _, handler in table.clone(handlers) do
				task.spawn(handler, ...)
			end
		end,
	}
end

local function urlDecode(text: string): string
	return (string.gsub(text, "%%(%x%x)", function(hex)
		return string.char(tonumber(hex, 16) :: number)
	end))
end

local function modelBase64(attribute: string): string
	local part = Instance.new("Part")
	part:SetAttribute("from", attribute)
	local blob = SerializationService:SerializeInstancesAsync({ part })
	part:Destroy()
	return buffer.tostring(EncodingService:Base64Encode(blob))
end

-- The fake daemon. `nodes` is the tree; `delta`, what the next /changes answers.
local function newDaemon()
	local daemon = {
		epoch = "e1",
		version = 1,
		nodes = {},
		delta = nil,
		sent = {}, -- what the plugin said on its socket
		saves = {},
		backups = {},
		trees = 0,
		failModel = {},
		onOpen = nil, -- messages to push as the socket opens, before it reports Opened
		socket = nil,
	}

	function daemon.push(message)
		daemon.socket.MessageReceived:fire(RealHttp:JSONEncode(message))
	end

	function daemon.said(kind: string): { any }
		local found = {}
		for _, message in daemon.sent do
			if message.type == kind then
				table.insert(found, message)
			end
		end
		return found
	end

	function daemon.nextSaid(kind: string, after: number): any
		return untilTrue(function()
			return daemon.said(kind)[after + 1]
		end)
	end

	local function answer(code: number, value: any)
		return { StatusCode = code, Body = if type(value) == "string" then value else RealHttp:JSONEncode(value) }
	end

	function daemon.RequestAsync(_, request: any)
		local route = string.match(request.Url, "^%a+://[^/]+([^?]*)")
		if route == "/tree" then
			daemon.trees += 1
			return answer(200, {
				name = "harness",
				epoch = daemon.epoch,
				version = daemon.version,
				writeback = true,
				roots = { { "ServerStorage", ROOT } },
				nodes = daemon.nodes,
			})
		elseif route == "/changes" then
			local epoch = string.match(request.Url, "[?&]epoch=([^&]*)")
			if epoch ~= daemon.epoch or not daemon.delta then
				return answer(200, { full = true })
			end
			local delta = daemon.delta
			daemon.delta = nil
			return answer(200, {
				full = false,
				version = daemon.version,
				upserts = delta.upserts or {},
				removes = delta.removes or {},
			})
		elseif route == "/model" then
			local file = urlDecode(string.match(request.Url, "[?&]file=([^&]*)"))
			if daemon.failModel[file] then
				return answer(500, "disk says no")
			end
			return answer(200, modelBase64(file))
		elseif route == "/save" then
			local mode = string.match(request.Url, "[?&]mode=([^&]*)")
			local path = urlDecode(string.match(request.Url, "[?&]path=([^&]*)"))
			table.insert(daemon.saves, { mode = mode, path = path })
			return answer(200, { ok = true, action = if mode == "edit" then "saved" else "none", modelHash = "same" })
		elseif route == "/backup" then
			for _, item in RealHttp:JSONDecode(request.Body).items do
				table.insert(daemon.backups, table.concat(item.path, "."))
			end
			return answer(200, { dir = "backups/x" })
		end
		error(`the harness has no route {route}`)
	end

	function daemon.CreateWebStreamClient(_, _, options: any)
		local socket = {
			url = options.Url,
			Opened = newSignal(),
			Error = newSignal(),
			Closed = newSignal(),
			MessageReceived = newSignal(),
		}
		function socket.Send(_, text: string)
			table.insert(daemon.sent, RealHttp:JSONDecode(text))
		end
		function socket.Close(self)
			self.Closed:fire()
		end
		daemon.socket = socket
		task.defer(function()
			if daemon.onOpen then
				for _, message in daemon.onOpen do
					daemon.push(message)
				end
				daemon.onOpen = nil
				task.wait()
			end
			socket.Opened:fire()
		end)
		return socket
	end

	function daemon.JSONEncode(_, value)
		return RealHttp:JSONEncode(value)
	end
	function daemon.JSONDecode(_, value)
		return RealHttp:JSONDecode(value)
	end
	function daemon.UrlEncode(_, value)
		return RealHttp:UrlEncode(value)
	end
	function daemon.GenerateGUID(_, wrap)
		return RealHttp:GenerateGUID(wrap)
	end

	return daemon
end

local function loadSync(daemon: any): any
	local source = ServerStorage.SyncUnderTest.Source
	local line = 'local HttpService = game:GetService("HttpService")'
	local at, last = string.find(source, line, 1, true)
	assert(at, "Sync.luau no longer gets HttpService this way")
	local patched = string.sub(source, 1, at - 1) .. "local HttpService = ..." .. string.sub(source, last + 1)
	return assert(loadstring(patched))(daemon)
end

local function scriptNode(name: string, source: string, kind: string?): any
	return {
		path = { "ServerStorage", ROOT, name },
		className = "ModuleScript",
		kind = kind or "script",
		source = source,
		hash = source,
	}
end

local function modelNode(name: string): any
	return {
		path = { "ServerStorage", ROOT, name },
		className = "Part",
		kind = "model",
		model = `{name}.rbxm`,
		modelHash = "same",
		hash = name,
	}
end

local function baseNodes(): { any }
	return {
		{ path = { "ServerStorage", ROOT }, className = "Folder", kind = "project", hash = "root" },
		scriptNode("a", "return 'a'\n"),
		scriptNode("generated", "return 'ours'\n", "generated"),
		modelNode("m"),
	}
end

local function clean()
	local root = ServerStorage:FindFirstChild(ROOT)
	if root then
		root:Destroy()
	end
end

local function scenario(name: string, body: (any, any, any) -> ())
	clean()
	local daemon = newDaemon()
	daemon.nodes = baseNodes()
	local Sync = loadSync(daemon)
	local logs = {}
	local s
	local ok, err = xpcall(function()
		body(daemon, Sync, function(prepared: (() -> ())?)
			if prepared then
				prepared()
			end
			s = Sync.start({
				base = "http://fake",
				placeId = 1,
				log = function(line)
					table.insert(logs, line)
				end,
			})
			check(
				`{name}: connects`,
				untilTrue(function()
					return s.status == "connected"
				end, 10)
			)
			task.wait(0.1) -- hooks arm a frame later
			return s, logs
		end)
	end, debug.traceback)
	if not ok then
		table.insert(failures, `{name}: {err}`)
	end
	if s then
		Sync.stop(s)
	end
	clean()
end

local function child(name: string): Instance?
	local root = ServerStorage:FindFirstChild(ROOT)
	return root and root:FindFirstChild(name)
end

-- A version the daemon announces as the socket opens is applied, not lost
scenario("early version", function(daemon, _, start)
	daemon.version = 2
	daemon.onOpen = { { type = "version", epoch = "e1", version = 3 } }
	-- the tree is v2; the socket says v3 before the plugin counts it as open
	local node = scriptNode("late", "return 'late'\n")
	daemon.delta = { upserts = { node } }
	local realOpen = daemon.CreateWebStreamClient
	function daemon.CreateWebStreamClient(self, kind, options)
		daemon.version = 3
		return realOpen(self, kind, options)
	end
	start()
	check(
		"early version: applied",
		untilTrue(function()
			local applied = daemon.said("applied")[1]
			return applied and applied.version == 3
		end),
		RealHttp:JSONEncode(daemon.sent)
	)
	check("early version: its change is in Studio", child("late"))
end)

-- Studio edits of a model save
scenario("model edits", function(daemon, _, start)
	start()
	local m = child("m") :: Instance
	daemon.saves = {}
	CollectionService:AddTag(m, "StudioSyncHarnessTag")
	check(
		"a tag edit saves the model",
		untilTrue(function()
			return #daemon.saves > 0
		end, 4)
	)
	check("as an edit", daemon.saves[1] and daemon.saves[1].mode == "edit")
	task.wait(1.5)
	daemon.saves = {}
	local kept = Instance.new("Folder")
	kept.Name = "kept"
	kept.Parent = m
	task.wait(1.5)
	CollectionService:AddTag(kept, "StudioSyncHarnessTag")
	task.wait(1.5)
	daemon.saves = {}
	CollectionService:RemoveTag(kept, "StudioSyncHarnessTag")
	check(
		"removing a tag from a descendant saves too",
		untilTrue(function()
			return #daemon.saves > 0
		end, 4)
	)
	CollectionService:RemoveTag(m, "StudioSyncHarnessTag")
	task.wait(1.5)

	-- a flush saves at once, without the quiet time, and says so after
	daemon.saves = {}
	m:SetAttribute("edited", true)
	task.wait()
	daemon.push({ type = "flush", seq = 7 })
	local flushed = daemon.nextSaid("flushed", 0)
	check("flushed", flushed and flushed.seq == 7)
	check("flush saved first", #daemon.saves == 1, #daemon.saves)
end)

-- What the sync made and something removed comes back with a full sync
scenario("missing", function(daemon, _, start)
	start()
	local trees = daemon.trees
	daemon.push({ type = "verify", seq = 1 })
	local clean1 = daemon.nextSaid("verified", 0)
	check(
		"nothing is wrong at first",
		clean1 and #clean1.missing == 0 and #clean1.edited == 0,
		RealHttp:JSONEncode(clean1)
	)
	check("and no full sync", daemon.trees == trees)

	for label, damage in
		{
			removed = function(a)
				a:Destroy()
			end,
			renamed = function(a)
				a.Name = "renamed"
			end,
			moved = function(a)
				a.Parent = ServerStorage
			end,
		}
	do
		local a = child("a") :: Instance
		damage(a)
		local said, applied = #daemon.said("verified"), #daemon.said("applied")
		daemon.push({ type = "verify", seq = 2 })
		local verified = daemon.nextSaid("verified", said)
		check(
			`{label}: reported`,
			verified and table.find(verified.missing, `ServerStorage.{ROOT}.a`),
			RealHttp:JSONEncode(verified)
		)
		check(`{label}: full sync, then applied`, daemon.nextSaid("applied", applied))
		local back = child("a")
		check(`{label}: back`, back and (back :: any).Source == "return 'a'\n")
		if a.Parent == ServerStorage then
			a:Destroy()
		end
	end
	local stray = ServerStorage:FindFirstChild("a")
	check("the moved one is cleaned up by this test", stray == nil)
end)

-- Script text that differs is reported, left alone, and replaced (with a backup) by a resync
scenario("edited text", function(daemon, _, start)
	start()
	local a = child("a") :: any
	a.Source = "return 'edited in Studio'\n"
	local trees = daemon.trees
	daemon.push({ type = "verify", seq = 1 })
	local verified = daemon.nextSaid("verified", 0)
	check("reported", verified and table.find(verified.edited, `ServerStorage.{ROOT}.a`), RealHttp:JSONEncode(verified))
	check("not missing", verified and #verified.missing == 0)
	task.wait(0.2)
	check("left alone", a.Source == "return 'edited in Studio'\n" and daemon.trees == trees)

	daemon.epoch, daemon.version = "e2", 5
	daemon.push({ type = "version", epoch = "e2", version = 5 })
	check(
		"resync applied",
		untilTrue(function()
			local applied = daemon.said("applied")[1]
			return applied and applied.version == 5
		end)
	)
	check("replaced", a.Source == "return 'a'\n")
	check("backed up", table.find(daemon.backups, `ServerStorage.{ROOT}.a`), table.concat(daemon.backups, ","))
end)

-- A full sync backs up work, not modules generated from data files
scenario("backups", function(daemon, _, start)
	start(function()
		local root = Instance.new("Folder")
		root.Name = ROOT
		for name, source in { a = "hand work", generated = "return 'as Rojo words it'" } do
			local script = Instance.new("ModuleScript")
			script.Name = name
			script.Source = source
			script.Parent = root
		end
		root.Parent = ServerStorage
	end)
	check("work is backed up", table.find(daemon.backups, `ServerStorage.{ROOT}.a`), table.concat(daemon.backups, ","))
	check("generated is not", not table.find(daemon.backups, `ServerStorage.{ROOT}.generated`))
	check("generated is replaced", (child("generated") :: any).Source == "return 'ours'\n")
end)

-- A failure stays reported through later deltas, and a flush retries it
scenario("failed node", function(daemon, _, start)
	daemon.failModel["bad.rbxm"] = true
	table.insert(daemon.nodes, modelNode("bad"))
	start()
	local function lastReport()
		local reports = daemon.said("report")
		return reports[#reports]
	end
	check(
		"reported",
		untilTrue(function()
			local report = lastReport()
			return report and #report.errors > 0
		end),
		RealHttp:JSONEncode(lastReport())
	)
	check("not there", child("bad") == nil)

	daemon.version = 2
	daemon.delta = { upserts = { scriptNode("b", "return 'b'\n") } }
	daemon.push({ type = "version", epoch = "e1", version = 2 })
	check(
		"a delta of other files applies",
		untilTrue(function()
			return child("b")
		end)
	)
	task.wait(0.1)
	check("the failure stays reported", #lastReport().errors > 0, RealHttp:JSONEncode(lastReport()))

	daemon.failModel["bad.rbxm"] = nil
	daemon.push({ type = "flush", seq = 1 })
	check("flushed", daemon.nextSaid("flushed", 0))
	check("the retry brought it", child("bad"))
	check("and cleared the report", #lastReport().errors == 0, RealHttp:JSONEncode(lastReport()))
end)

-- A check stays cheap on a big project, and waits for a sync in progress
scenario("big project", function(daemon, _, start)
	for i = 1, 3000 do
		table.insert(daemon.nodes, scriptNode(`s{i}`, `return {i}\n`))
	end
	start()
	local began = os.clock()
	daemon.push({ type = "verify", seq = 1 })
	local verified = daemon.nextSaid("verified", 0)
	local ms = (os.clock() - began) * 1000
	check("big: clean", verified and #verified.missing == 0 and #verified.edited == 0, RealHttp:JSONEncode(verified))
	check("big: under 200 ms", ms < 200, ms)
	failures.bigMs = math.floor(ms)

	-- more than the names a message carries
	local root = ServerStorage:FindFirstChild(ROOT) :: Instance
	for i = 1, 30 do
		(root:FindFirstChild(`s{i}`) :: any).Source = "edited"
	end
	daemon.push({ type = "verify", seq = 2 })
	local capped = daemon.nextSaid("verified", 1)
	check(
		"names are capped",
		capped and #capped.edited == 21 and capped.edited[21] == "...and 10 more",
		RealHttp:JSONEncode(capped)
	)
end)

-- A Play holds a socket of its own, and says nothing on it
scenario("play", function(daemon, Sync)
	local socket = Sync.announcePlay({ base = "http://fake", placeId = 1, session = "abc123" })
	check("play: opens /play", socket and string.find(socket.url, "ws://fake/play?", 1, true), socket and socket.url)
	check("play: names its session", socket and string.find(socket.url, "session=abc123", 1, true))
	check("play: says nothing", #daemon.sent == 0)

	-- a copy's Play client gets the place's name late
	local asks = 0
	local late = Sync.announcePlay({
		base = "http://fake",
		placeId = 0,
		placeName = function()
			asks += 1
			return if asks < 3 then "Game" else "x-copy-abc123.rbxl"
		end,
	})
	check("play: waits for a copy's name", asks >= 3 and late and string.find(late.url, "session=abc123", 1, true), late and late.url)
end)

local bigMs = failures.bigMs
failures.bigMs = nil
if #failures == 0 then
	return `all {passed} checks passed (a check of 3000 scripts: {bigMs} ms)`
end
return `{passed} passed, {#failures} FAILED:\n{table.concat(failures, "\n")}`
