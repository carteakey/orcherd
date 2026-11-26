// Orcherd team board for the Herdr desktop app (herdr-app).
//
// Board state (team, seats, task DAG, decision gates, unread counts) comes from
// `orcherd status --json`, run from the shown space's folder so the panel follows the
// repo you're looking at. `status` is a read-only op: polling it writes no audit rows.
// Live seat status comes from herdr-core itself: seats that `orcherd spawn` starts carry
// `orcherd.team` / `orcherd.seat` labels, which `terminals()` reports without polling.

import { View, div, svg } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { exec, spaces, terminals, theme, run, checking, nextCommand, notify, window, home } from "herdr";

const POLL_MS = 5000;
const ICON = "mods/orcherd/icon.svg";

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

  // The shown space's folder when it's on this machine: orcherd keeps its store locally.
  folder() {
    const s = spaces().find((x) => x.active);
    return s && !s.machine && s.folder ? s.folder : home();
  }

  refresh(cx) {
    if (this.inflight) return;
    this.inflight = true;
    const cwd = this.folder();
    cx.spawn(async (cx) => {
      try {
        const r = await exec("orcherd", ["status", "--json"], { cwd, timeout: 10000 });
        if (r.code === 0) {
          const receipt = JSON.parse(r.stdout);
          this.proj = receipt?.data?.value ?? null;
          this.error = null;
          this.announce(this.proj);
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
        this.cwd = cwd;
        this.loading = false;
        this.inflight = false;
      }
      cx.notify();
    });
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
    return term ? row.on_click(() => run("terminal.peek", { terminal: term.address })) : row;
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

  render(cx) {
    const t = theme();
    if (this.loading) return this.empty(t, "Orcherd", "Loading team board…");
    if (this.error) return this.empty(t, "Orcherd unavailable", this.error);
    const p = this.proj;
    if (!p?.team) return this.empty(t, "No Orcherd team here", "Run `orcherd init --name <team> --template review-crew` in this repo.");
    // Follow the shown space: refetch when it moved to another folder.
    if (this.cwd !== this.folder()) this.refresh(cx);

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

    let body = v_flex().gap(14);
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

    return v_flex().size_full().bg(t.surface).text_size(12.5)
      .child(v_flex().px(12).pt(12).pb(10).gap(6).border_b(1).border_color(t.border)
        .child(h_flex().gap(8).items_center()
          .child(svg(ICON).size(16).text_color(t.accent))
          .child(div().text_color(t.foreground).font_semibold().truncate().child(p.team.name))
          .child(div().flex_1())
          .child(div().id("refresh").text_size(11).text_color(t.dim).hover((s) => s.text_color(t.foreground))
            .on_click((_e, cx) => this.refresh(cx)).child("refresh")))
        .when(Boolean(p.team.goal), (el) => el.child(div().text_size(11.5).text_color(t.muted).child(p.team.goal)))
        .child(h_flex().gap(6)
          .child(this.pill(t, `${working} working`, working ? t.working : t.dim))
          .child(this.pill(t, `${blocked} blocked`, blocked ? t.blocked : t.dim))
          .child(this.pill(t, `${p.pending_gates.length} gates`, p.pending_gates.length ? t.working : t.dim))
          .when(Boolean(p.team.paused_at), (el) => el.child(this.pill(t, "paused", t.blocked)))))
      .child(div().id("board").flex_1().overflow_y_scrollbar().p(8).child(body));
  }
}
