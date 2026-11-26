--!strict
-- Orcherd UI Mod for Herdr / Sheprd:
-- Renders live agent seats, blocked/gate alerts, and workspace roster across
-- the right-hand mod panel, status bar, and activity bar app.
local ui = require("@herdr/ui")

local STATUS_TONE: { [string]: string } = {
	blocked = "attention",
	working = "warning",
	done = "success",
	idle = "muted",
}

return herdr.mod({
	watch = { "hosts", "workspaces", "agents", "focus" },

	init = function()
		return {
			only_attention = false,
			last_refresh = 0,
		}
	end,

	update = function(state, msg)
		if msg == "toggle-filter" or msg == "command:toggle-filter" then
			state.only_attention = not state.only_attention
		elseif msg == "refresh" or msg == "command:refresh" then
			state.last_refresh = herdr.now() or 0
		end
		return state
	end,

	view = function(state, app)
		local all = app.agents.all
		local blocked = app.agents.blocked
		local working = app.agents.working
		local idle = app.agents.idle

		local seats = ui.list({})
		for _, agent in all do
			local show = not state.only_attention or agent.status == "blocked" or agent.status == "working"
			if show then
				table.insert(seats, ui.row({
					gap = "small",
					align = "center",
					ui.icon({ name = "terminal", tone = STATUS_TONE[agent.status] or "muted" }),
					ui.text({ text = agent.name, weight = if agent.status == "blocked" then "bold" else "normal", truncate = true }),
					ui.spacer(),
					ui.badge({ text = agent.status, tone = STATUS_TONE[agent.status] or "muted" }),
				}))
			end
		end

		local summary = ui.row({
			gap = "normal",
			align = "center",
			ui.badge({ text = tostring(#working) .. " working", tone = if #working > 0 then "warning" else "muted" }),
			ui.badge({ text = tostring(#blocked) .. " gated/blocked", tone = if #blocked > 0 then "attention" else "muted" }),
			ui.badge({ text = tostring(#idle) .. " idle", tone = "muted" }),
			ui.spacer(),
			ui.button({
				label = if state.only_attention then "Show all" else "Active only",
				msg = "toggle-filter",
			}),
		})

		local body = ui.column({
			gap = "normal",
			summary,
			ui.meter({
				value = if #all > 0 then (#working + #blocked) / #all else 0,
				label = "Active & gated seats",
				tone = if #blocked > 0 then "attention" else "accent",
			}),
		})

		if #seats == 0 then
			table.insert(body, ui.text({ text = "No matching seats in current filter.", tone = "muted", size = "small" }))
		else
			table.insert(body, seats)
		end

		return body
	end,
})
