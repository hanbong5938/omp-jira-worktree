--!strict
-- Window-only plugin: palette commands that open the jira-tern app in a pane.
-- APIs: tern.command, tern.plugin.dir, tern.getenv, cx.session:panes()/focused(),
-- cx.layout:new_tab/split with a launch table {cwd, command}, cx:toast
-- (docs.stencil.so/tern/guides/layout.md and guides/commands.md).

local function shell_quote(value: string): string
	return "'" .. string.gsub(value, "'", "'\\''") .. "'"
end

-- Focused pane's cwd; layout.md: panes() entries carry `cwd` ("" when unknown).
local function focused_cwd(cx): string
	local focused = cx.session:focused()
	if focused ~= nil then
		for _, p in cx.session:panes() do
			if p.pane == focused and p.cwd ~= "" then
				return p.cwd
			end
		end
	end
	return tern.getenv("HOME") or "/"
end

local function open(cx, split: boolean)
	local spec = {
		cwd = focused_cwd(cx),
		-- `command` runs through the login shell (layout.md, Launch tables).
		command = "exec /bin/sh " .. shell_quote(tern.plugin.dir .. "/scripts/jira-tern.sh"),
	}
	local pane
	if split then
		local focused = cx.session:focused()
		if focused ~= nil then
			pane = cx.layout:split(focused, "right", spec)
		end
	else
		pane = cx.layout:new_tab(spec)
	end
	if pane == nil then
		cx:toast("error", "Could not open Jira")
	end
end

tern.command({
	id = "open",
	title = "Jira: open (tab)",
	icon = "terminal",
	group = "Jira",
	run = function(cx)
		open(cx, false)
	end,
})

tern.command({
	id = "open-split",
	title = "Jira: open (split right)",
	icon = "terminal",
	group = "Jira",
	run = function(cx)
		open(cx, true)
	end,
})
