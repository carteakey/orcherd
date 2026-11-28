// Orcherd team board for the Herdr desktop app (herdr-app).
//
// Board state (team, seats, task DAG, decision gates, unread counts) comes from
// `orcherd status --json`, run from the shown space's folder so the panel follows the
// repo you're looking at. `status` is a read-only op: polling it writes no audit rows.
// Live seat status comes from herdr-core itself: seats that `orcherd spawn` starts carry
// `orcherd.team` / `orcherd.seat` labels, which `terminals()` reports without polling.
// The same labels group every live orcherd agent on this machine by team ("Teams"), so
// Herdr's own Agents list stays as shipped.

import { View, div, svg } from "gpui-kit";
import { v_flex, h_flex, TextView } from "gpui-base";
import { exec, spaces, focused, terminals, theme, run, checking, nextCommand, notify, window, home } from "herdr";

const POLL_MS = 5000;
const BULLETIN_SHOWN = 40; // newest first; the latest BULLETIN_OPEN show their full text
const BULLETIN_OPEN = 3;
const COLUMN_MIN = 360; // each column's narrowest width
const CONFIRM_MS = 4000; // the Freeze button arms on the first click and fires on the second
const ICON = "mods/orcherd/icon.svg";
const TEAM_SPACE_PREFIX = "orcherd-"; // `orcherd spawn` opens each team's seats in space `orcherd-<team id>`

// Most urgent first; completed work folds into a count at the end.
const TASK_ORDER = ["blocked", "review", "in_progress", "assigned", "ready", "pending", "failed"];
const TASK_LABEL = {
  blocked: "Blocked", review: "In review", in_progress: "In progress", assigned: "Assigned",
  ready: "Ready", pending: "Waiting on deps", failed: "Failed",
};
const ROLE_ORDER = { supervisor: 0, reviewer: 1, planner: 2, worker: 3 };

const statusColor = (t, s) =>
  ({ blocked: t.blocked, working: t.working, done: t.done, idle: t.idle })[s] ?? t.dim;

function since(iso) {
  const ms = Date.now() - Date.parse(iso ?? "");
  if (!Number.isFinite(ms) || ms < 0) return "";
  const m = Math.floor(ms / 60000);
  if (m < 1) return "now";
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return h < 24 ? `${h}h` : `${Math.floor(h / 24)}d`;
}

export default class Orcherd extends View {
  init(_props, cx) {
    this.proj = null;
    this.error = null;
    this.loading = true;
    this.cwd = null;
    this.showDone = false;
    this.seenGates = null; // gate ids already announced; null until the first load
    this.posts = [];
    this.openPosts = new Set(); // post ids expanded by a click
    this.seenPosts = null; // post ids present when the board opened; later ones are "new"
    this.armedAt = 0; // when the Freeze/Unfreeze button was armed
    this.acting = false;
    this.teamNames = new Map(); // team id -> name, looked up once per team
    if (checking()) return;
    this.refresh(cx);
    cx.timer.every(POLL_MS, (cx) => {
      // Pause while the window isn't presented; render() restarts on return.
      if (window()?.visible === false) return;
      this.refresh(cx);
    });
    cx.spawn(async (cx) => {
      for (;;) {
        const c = await nextCommand();
        if (c === "refresh") this.refresh(cx);
      }
    });
  }

  // The folder you're looking at, on this machine (orcherd keeps its store locally): the
  // focused terminal's, else the shown space's. A space's own folder is often just home.
  folder() {
    const f = focused();
    if (f?.cwd && !String(f.terminal ?? "").includes("/")) return f.cwd;
    const s = spaces().find((x) => x.active && !x.machine);
    return s?.folder || home();
  }

  // Names for the team ids on live terminals' labels. `status` is read-only; each id is
  // asked once (a team from another store stays shown by id).
  async nameTeams() {
    const ids = new Set(terminals().map((t) => t.labels?.["orcherd.team"]).filter(Boolean));
    for (const id of ids) {
      if (this.teamNames.has(id)) continue;
      const r = await exec("orcherd", ["--team", id, "status", "--json"], { cwd: home(), timeout: 10000 }).catch(() => null);
      this.teamNames.set(id, r?.code === 0 ? JSON.parse(r.stdout)?.data?.value?.team?.name ?? null : null);
    }
  }

  // Each team's seats share a space; Herdr names an unnamed space after its repo, so name it
  // after the team instead. Once named (by us or by hand), Herdr keeps the name.
  nameTeamSpaces() {
    for (const s of spaces()) {
      if (s.machine || s.named || !s.id.startsWith(TEAM_SPACE_PREFIX)) continue;
      const name = this.teamNames.get(s.id.slice(TEAM_SPACE_PREFIX.length));
      if (name) run("space.rename", { id: s.id, name });
    }
  }

  // The team in view, when the window says it outright: the focused terminal's own
  // `orcherd.team` label (a seat), else the shown space's id (`orcherd-<team id>`). Seats
  // live in worktrees outside the repo, so the folder alone can't tell.
  teamInView() {
    const f = focused();
    const label = terminals().find((x) => x.address === f?.terminal)?.labels?.["orcherd.team"];
    if (label) return label;
    const space = spaces().find((x) => x.active && !x.machine);
    return space?.id?.startsWith(TEAM_SPACE_PREFIX) ? space.id.slice(TEAM_SPACE_PREFIX.length) : null;
  }

  // What the board is showing: a team, or a folder to resolve one from.
  viewKey() {
    return this.teamInView() ?? this.folder();
  }

  refresh(cx) {
    if (this.inflight) return;
    this.inflight = true;
    const cwd = this.folder();
    const team = this.teamInView();
    const key = team ?? cwd;
    cx.spawn(async (cx) => {
      try {
        const r = await exec("orcherd", team ? ["--team", team, "status", "--json"] : ["status", "--json"], { cwd, timeout: 10000 });
        if (r.code === 0) {
          const receipt = JSON.parse(r.stdout);
          this.proj = receipt?.data?.value ?? null;
          // Only a team whose workspace holds this folder: with no match orcherd falls back to
          // the newest team in the store, which belongs to some other repo.
          const root = this.proj?.team?.workspace_root;
          if (!team && root && cwd !== root && !cwd.startsWith(root.endsWith("/") ? root : `${root}/`)) this.proj = null;
          this.error = null;
          this.announce(this.proj);
          // Bulletin: a read-only op, so polling it writes no audit rows.
          const b = this.proj && await exec("orcherd", ["--team", this.proj.team.team_id, "bulletin", "list", "--json"], { cwd, timeout: 10000 });
          if (b?.code === 0) {
            this.posts = (JSON.parse(b.stdout)?.data?.value ?? []).slice(-BULLETIN_SHOWN).reverse();
            this.seenPosts ??= new Set(this.posts.map((post) => post.post_id));
          }
        } else {
          const msg = (r.stderr || r.stdout).trim();
          // No team yet is a normal state, not an error.
          this.proj = null;
          this.error = /not found.*team|team.*not found/i.test(msg) ? null : msg.split("\n")[0];
        }
      } catch (e) {
        this.proj = null;
        this.error = `orcherd is not runnable: ${e?.message ?? e}`;
      } finally {
        await this.nameTeams().catch(() => {});
        this.nameTeamSpaces();
        this.cwd = cwd;
        this.key = key;
        this.loading = false;
        this.inflight = false;
      }
      cx.notify();
    });
  }

  // Freeze or unfreeze the team as the human at this window (the app runs without a seat).
  toggleFreeze(cx) {
    const p = this.proj;
    if (!p || this.acting) return;
    if (Date.now() - this.armedAt > CONFIRM_MS) {
      this.armedAt = Date.now();
      cx.notify();
      return;
    }
    this.armedAt = 0;
    this.acting = true;
    const args = p.team.paused_at
      ? ["--team", p.team.team_id, "unfreeze"]
      : ["--team", p.team.team_id, "freeze", "--reason", "Frozen from the Herdr app board"];
    cx.spawn(async (cx) => {
      const r = await exec("orcherd", args, { cwd: this.cwd ?? home(), timeout: 30000 }).catch((e) => ({ code: 1, stderr: String(e) }));
      notify({
        title: r.code === 0 ? (p.team.paused_at ? "Team unfrozen" : "Team frozen") : "Orcherd: freeze failed",
        body: (r.code === 0 ? r.stdout : r.stderr || r.stdout).trim().split("\n").slice(0, 2).join(" "),
        level: r.code === 0 ? "info" : "error",
        icon: ICON,
      });
      this.acting = false;
      this.refresh(cx);
    });
  }

  // A post's markdown, led by its subject unless the subject is just the body's opening.
  postText(post) {
    const plain = (x) => x.replace(/[*_`#>]/g, "").trim();
    const subject = plain(post.subject ?? "");
    if (!subject || plain(post.body).startsWith(subject.replace(/…$/, ""))) return post.body;
    return `**${post.subject}**\n\n${post.body}`;
  }

  postRow(t, post, index) {
    const tone = post.kind === "authorized" ? t.done : post.kind === "handoff" ? t.working : t.muted;
    const isNew = this.seenPosts && !this.seenPosts.has(post.post_id);
    const open = index < BULLETIN_OPEN || this.openPosts.has(post.post_id);
    const title = post.subject || post.body.split("\n")[0];
    return v_flex().id(`post-${post.post_id}`).px(8).py(6).gap(3).rounded(6)
      .hover((s) => s.bg(t.hover))
      .on_click((_e, cx) => {
        if (index < BULLETIN_OPEN) return;
        this.openPosts.has(post.post_id) ? this.openPosts.delete(post.post_id) : this.openPosts.add(post.post_id);
        cx.notify();
      })
      .child(h_flex().gap(6).items_center().text_size(11)
        .child(div().text_color(t.foreground).font_semibold().child(post.author_seat))
        .when(post.kind !== "post", (el) => el.child(this.pill(t, post.kind, tone)))
        .when(isNew, (el) => el.child(this.pill(t, "new", t.accent)))
        .child(div().flex_1())
        .child(div().text_color(t.dim).child(since(post.created_at))))
      .child(open
        ? TextView.markdown(`post-body-${post.post_id}`, this.postText(post)).raw_html(false).selectable(true)
        : div().text_color(t.muted).truncate().child(title));
  }


  // A toast when a seat asks the human-facing supervisor something, once per gate.
  announce(proj) {
    const gates = proj?.pending_gates ?? [];
    const first = this.seenGates === null;
    this.seenGates ??= new Set();
    for (const g of gates) {
      if (this.seenGates.has(g.gate_id)) continue;
      this.seenGates.add(g.gate_id);
      if (first) continue;
      notify({
        title: `Orcherd: ${g.asked_by_seat} asks ${g.to_seat}`,
        body: g.question,
        level: g.kind === "escalation" ? "warning" : "info",
        icon: ICON,
        action: { name: "mod.open", args: { name: "orcherd" } },
      });
    }
  }

  // Live terminal for a seat: by orcherd labels first, then by its recorded address.
  terminalFor(seat, teamId, all) {
    const live = all.filter((t) => t.state === "live" || t.state === undefined);
    const byLabel = live.find((t) => t.labels?.["orcherd.team"] === teamId && t.labels?.["orcherd.seat"] === seat.name);
    if (byLabel) return byLabel;
    const a = seat.address;
    if (a?.kind !== "core") return null;
    const addr = a.machine ? `${a.machine}/${a.terminal}` : a.terminal;
    return live.find((t) => t.address === addr || t.address === a.terminal) ?? null;
  }

  section(t, title, count, body) {
    return v_flex().gap(4)
      .child(h_flex().gap(6).items_center().text_size(11).text_color(t.dim).font_semibold()
        .child(title.toUpperCase()).when(count !== null, (el) => el.child(div().text_color(t.muted).child(String(count)))))
      .child(body);
  }

  pill(t, text, color) {
    return div().px(6).py(1).rounded(4).text_size(10.5).border(1).border_color(t.border).text_color(color ?? t.muted).child(text);
  }

  seatRow(t, seat, term, unread, task) {
    const status = term ? (term.status === "error" ? "blocked" : term.status ?? "idle") : null;
    const row = h_flex().id(`seat-${seat.seat_id}`).gap(8).items_center().px(8).py(5).rounded(6)
      .hover((s) => s.bg(t.hover))
      .child(div().size(8).rounded(4).flex_none().bg(status ? statusColor(t, status) : t.border))
      .child(v_flex().flex_1().min_w(0)
        .child(h_flex().gap(6).items_center()
          .child(div().text_color(t.foreground).font_semibold().truncate().child(seat.name))
          .child(this.pill(t, seat.role, seat.role === "supervisor" ? t.accent : seat.role === "reviewer" ? t.done : t.muted)))
        .child(div().text_size(11).text_color(t.dim).truncate()
          .child(task ? task.title : status ? `${seat.agent} · ${status}` : `${seat.agent} · not running`)))
      .when(unread > 0, (el) => el.child(div().px(6).rounded(8).bg(t.accent).text_color(t.accent_foreground).text_size(10.5).child(String(unread))));
    if (!term) return row;
    // Seats opened as tabs get focused; headless seats get a peek.
    const cmd = term.labels?.["herdr.space"] ? "pane.focus" : "terminal.peek";
    return row.on_click(() => run(cmd, { terminal: term.address }));
  }

  taskRow(t, task, seatName) {
    const people = [task.assignee_seat && seatName(task.assignee_seat), task.reviewer_seat && `↳ ${seatName(task.reviewer_seat)}`].filter(Boolean).join("  ");
    return v_flex().px(8).py(4).gap(1)
      .child(div().text_color(t.foreground).truncate().child(task.title))
      .child(h_flex().gap(8).text_size(11).text_color(t.dim)
        .child(div().truncate().child(people || "unassigned"))
        .when(task.deps?.length > 0 && task.status === "pending", (el) => el.child(`${task.deps.length} dep${task.deps.length === 1 ? "" : "s"}`))
        .when(task.review_round > 0, (el) => el.child(`round ${task.review_round}`))
        .child(div().flex_1())
        .child(since(task.updated_at)));
  }

  gateRow(t, g) {
    const tone = g.kind === "escalation" ? t.blocked : t.working;
    return v_flex().px(8).py(6).gap(3).rounded(6).border(1).border_color(tone)
      .child(h_flex().gap(6).items_center().text_size(11)
        .child(div().text_color(tone).font_semibold().child(g.kind.replace("_", " ")))
        .child(div().text_color(t.dim).child(`${g.asked_by_seat} → ${g.to_seat}`))
        .child(div().flex_1()).child(div().text_color(t.dim).child(since(g.created_at))))
      .child(div().text_color(t.foreground).child(g.question))
      .child(div().text_size(11).text_color(t.dim).child(
        g.kind.startsWith("approval") ? `orcherd approve ${g.gate_id}` : `orcherd reply ${g.gate_id} --body-file …`));
  }

  empty(t, title, body) {
    return v_flex().size_full().p(16).gap(8).items_center().justify_center().bg(t.surface).text_size(12.5)
      .child(svg(ICON).size(28).text_color(t.dim))
      .child(div().text_color(t.foreground).font_semibold().child(title))
      .child(div().text_color(t.muted).child(body));
  }

  // Every live orcherd agent on this machine, grouped by team. A click focuses the seat's
  // tab, or peeks at a headless seat.
  teamsSection(t, all, exceptTeam) {
    const groups = new Map();
    for (const term of all) {
      const team = term.labels?.["orcherd.team"];
      if (!team || team === exceptTeam || (term.state && term.state !== "live")) continue;
      (groups.get(team) ?? groups.set(team, []).get(team)).push(term);
    }
    if (groups.size === 0) return null;
    const rows = [...groups.entries()]
      .sort(([a], [b]) => (this.teamNames.get(a) ?? a).localeCompare(this.teamNames.get(b) ?? b))
      .map(([team, terms]) => v_flex().gap(1)
        .child(div().px(8).text_size(11).text_color(t.muted).font_semibold().child(this.teamNames.get(team) ?? team))
        .children(terms
          .sort((a, b) => (ROLE_ORDER[a.labels["orcherd.role"]] ?? 9) - (ROLE_ORDER[b.labels["orcherd.role"]] ?? 9))
          .map((term) => {
            const status = term.status === "error" ? "blocked" : term.status ?? "idle";
            const headless = !term.labels["herdr.space"];
            return h_flex().id(`agent-${term.address}`).gap(8).items_center().px(8).py(3).rounded(6)
              .hover((s) => s.bg(t.hover))
              .child(div().size(8).rounded(4).flex_none().bg(statusColor(t, status)))
              .child(div().flex_1().truncate().text_color(t.foreground).child(term.labels["orcherd.seat"] ?? term.address))
              .child(div().text_size(11).text_color(t.dim).child(headless ? `${status} · headless` : status))
              .on_click(() => run(headless ? "terminal.peek" : "pane.focus", { terminal: term.address }));
          })));
    return this.section(t, exceptTeam ? "Other teams" : "Teams", groups.size, v_flex().gap(8).children(rows));
  }

  refreshButton(t) {
    return div().id("refresh").text_size(11).text_color(t.dim).hover((s) => s.text_color(t.foreground))
      .on_click((_e, cx) => { this.refresh(cx); cx.notify(); })
      .child(this.inflight ? "refreshing…" : "refresh");
  }

  render(cx) {
    const t = theme();
    if (this.loading) return this.empty(t, "Orcherd", "Loading team board…");
    // Follow the window: refetch when the team or folder in view changed.
    if (this.key !== this.viewKey()) this.refresh(cx);
    if (this.error) return this.empty(t, "Orcherd unavailable", this.error);
    const p = this.proj;
    if (!p?.team) {
      const teams = this.teamsSection(t, terminals(), null);
      if (!teams) return this.empty(t, "No Orcherd team here", "Run `orcherd init --name <team> --template review-crew` in this repo.");
      return v_flex().size_full().bg(t.surface).text_size(12.5)
        .child(h_flex().px(12).pt(12).pb(10).gap(8).items_center().border_b(1).border_color(t.border)
          .child(div().flex_1().text_color(t.muted).child("No Orcherd team in this folder."))
          .child(this.refreshButton(t)))
        .child(div().id("board").flex_1().overflow_y_scrollbar().p(8).child(teams));
    }

    const all = terminals();
    const seatsById = new Map(p.seats.map((s) => [s.seat_id, s]));
    const seatName = (ref) => seatsById.get(ref)?.name ?? ref;
    const activeTask = new Map();
    for (const task of p.tasks) {
      if (task.assignee_seat && ["assigned", "in_progress", "blocked", "review"].includes(task.status)) {
        activeTask.set(seatName(task.assignee_seat), task);
      }
    }

    const seatRows = [...p.seats]
      .sort((a, b) => (ROLE_ORDER[a.role] ?? 9) - (ROLE_ORDER[b.role] ?? 9))
      .map((s) => ({ s, term: this.terminalFor(s, p.team.team_id, all) }));
    const live = seatRows.filter((r) => r.term);
    const working = live.filter((r) => r.term.status === "working").length;
    const blocked = live.filter((r) => r.term.status === "blocked" || r.term.status === "error").length;

    const byStatus = {};
    for (const task of p.tasks) (byStatus[task.status] ??= []).push(task);
    const done = (byStatus.completed ?? []).length + (byStatus.cancelled ?? []).length;
    const open = p.tasks.length - done;

    let body = v_flex().gap(14).flex_1().min_w(COLUMN_MIN);
    if (p.team.paused_at) {
      body = body.child(v_flex().px(10).py(8).gap(3).rounded(6).border(1).border_color(t.blocked)
        .child(div().text_color(t.blocked).font_semibold().child(`Frozen ${since(p.team.paused_at)} ago`))
        .child(div().text_color(t.foreground).child(p.team.paused_reason ?? "No reason given"))
        .child(div().text_size(11).text_color(t.dim).child("New spawns, claims and assignments are refused.")));
    }
    if (p.pending_gates.length > 0) {
      body = body.child(this.section(t, "Needs a decision", p.pending_gates.length,
        v_flex().gap(6).children(p.pending_gates.map((g) => this.gateRow(t, g)))));
    }
    body = body.child(this.section(t, "Seats", `${live.length}/${p.seats.length} live`,
      v_flex().gap(1).children(seatRows.map(({ s, term }) => this.seatRow(t, s, term, p.unread_counts?.[s.name] ?? 0, activeTask.get(s.name))))));
    const taskGroups = TASK_ORDER.filter((k) => byStatus[k]?.length).map((k) =>
      v_flex().gap(1)
        .child(div().px(8).text_size(11).text_color(k === "blocked" || k === "failed" ? t.blocked : k === "review" ? t.done : t.muted).child(`${TASK_LABEL[k]} · ${byStatus[k].length}`))
        .children(byStatus[k].map((task) => this.taskRow(t, task, seatName))));
    body = body.child(this.section(t, "Tasks", `${open} open`,
      v_flex().gap(8).children(taskGroups)
        .when(open === 0, (el) => el.child(div().px(8).text_color(t.dim).child("No open tasks.")))
        .when(done > 0, (el) => el.child(
          div().id("toggle-done").px(8).text_size(11).text_color(t.dim).hover((s) => s.text_color(t.foreground))
            .on_click((_e, cx) => { this.showDone = !this.showDone; cx.notify(); })
            .child(`${this.showDone ? "▾" : "▸"} ${done} completed`)))
        .when(this.showDone, (el) => el.children((byStatus.completed ?? []).map((task) => this.taskRow(t, task, seatName))))));
    const others = this.teamsSection(t, all, p.team.team_id);
    if (others) body = body.child(others);
    const fresh = this.seenPosts ? this.posts.filter((post) => !this.seenPosts.has(post.post_id)).length : 0;
    const bulletin = v_flex().gap(4).flex_1().min_w(COLUMN_MIN)
      .child(this.section(t, "Bulletin", fresh ? `${fresh} new` : `live · ${this.posts.length}`,
        this.posts.length
          ? v_flex().gap(2).children(this.posts.map((post, i) => this.postRow(t, post, i)))
          : div().px(8).text_color(t.dim).child("No posts yet.")));

    return v_flex().size_full().bg(t.surface).text_size(12.5)
      .child(v_flex().px(12).pt(12).pb(10).gap(6).border_b(1).border_color(t.border)
        .child(h_flex().gap(8).items_center()
          .child(svg(ICON).size(16).text_color(t.accent))
          .child(div().text_color(t.foreground).font_semibold().truncate().child(p.team.name))
          .child(div().flex_1())
          .child(div().id("freeze").text_size(11)
            .text_color(Date.now() - this.armedAt <= CONFIRM_MS ? t.blocked : t.dim)
            .hover((s) => s.text_color(t.blocked))
            .on_click((_e, cx) => this.toggleFreeze(cx))
            .child(this.acting ? "…" : Date.now() - this.armedAt <= CONFIRM_MS
              ? (p.team.paused_at ? "click to unfreeze" : "click to freeze all")
              : (p.team.paused_at ? "unfreeze" : "freeze")))
          .child(this.refreshButton(t)))
        .when(Boolean(p.team.goal), (el) => el.child(div().text_size(11.5).text_color(t.muted).child(p.team.goal)))
        .child(h_flex().gap(6)
          .child(this.pill(t, `${working} working`, working ? t.working : t.dim))
          .child(this.pill(t, `${blocked} blocked`, blocked ? t.blocked : t.dim))
          .child(this.pill(t, `${p.pending_gates.length} gates`, p.pending_gates.length ? t.working : t.dim))
          .when(Boolean(p.team.paused_at), (el) => el.child(this.pill(t, "frozen", t.blocked)))))
      // Two columns that scroll on their own, so the bulletin stays in view beside the board.
      .child(h_flex().flex_1().min_h(0).gap(16).px(8)
        .child(div().id("board").flex_1().min_w(COLUMN_MIN).h_full().overflow_y_scrollbar().py(8).child(body))
        .child(div().id("bulletin").flex_1().min_w(COLUMN_MIN).h_full().overflow_y_scrollbar().py(8).child(bulletin)));
  }
}
