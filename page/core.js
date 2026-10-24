// The dashboard page's logic that does not touch the DOM: loading the active generation,
// what each tab lists, the summary tiles, Trends, and the page documents it writes. A classic
// script, so the page loads it with <script src> and node's tests with require().
(function (root) {
  "use strict";

  const PRIORITIES = ["p0", "p1", "p2", "p3"];
  // The tracker's generation collections (spec 10.10, publish.rs COLLECTIONS).
  const COLLECTIONS = ["issues", "acks", "launch_status", "suggestions", "plans", "forks", "metrics"];
  const LIST_LIMIT = 1000;
  const GENERATION = /^gen-[0-9]+$/;
  // How long a generation keeps a decision's ack (derive.rs ACK_DAYS).
  const ACK_DAYS = 30;

  // Reads the generation `meta/active` names. A sync run may switch generations and delete
  // the old one while this read is under way, so a generation that comes back short of its
  // marker's counts (or without a marker) sends the read back to `meta/active`. Returns
  // { status: "ok" | "empty" | "partial", meta, generation, data, attempts, why }.
  async function loadGeneration(db, opts) {
    const attempts = (opts && opts.attempts) || 3;
    let last = null;
    for (let attempt = 1; attempt <= attempts; attempt++) {
      const active = await db.doc("meta/active").get();
      if (!active.exists) return { status: "empty", attempts: attempt };
      const meta = active.data();
      const generation = meta && meta.generation;
      if (typeof generation !== "string" || !GENERATION.test(generation)) {
        return { status: "partial", meta, why: "meta/active names no generation", attempts: attempt, data: emptyData() };
      }
      const marker = await db.doc("gens/" + generation).get();
      if (!marker.exists) {
        last = { meta, generation, why: generation + " has no marker", data: emptyData(), attempts: attempt };
        continue;
      }
      const counts = (marker.data() || {}).counts || null;
      const data = emptyData();
      let why = null;
      for (const c of COLLECTIONS) {
        const want = counts && typeof counts[c] === "number" ? counts[c] : null;
        if (want === 0) continue;
        const snap = await db.collection("gens/" + generation + "/" + c).limit(LIST_LIMIT).get();
        data[c] = snap.docs.filter((d) => d.exists).map((d) => d.data());
        if (want !== null && data[c].length < Math.min(want, LIST_LIMIT)) {
          why = generation + "/" + c + " has " + data[c].length + " of " + want;
          break;
        }
        if (want !== null && want > LIST_LIMIT) why = generation + "/" + c + " holds more than " + LIST_LIMIT;
      }
      if (why === null) return { status: "ok", meta, generation, data, attempts: attempt };
      last = { meta, generation, why, data, attempts: attempt };
      if (why.endsWith("more than " + LIST_LIMIT)) break;
    }
    return Object.assign({ status: "partial" }, last);
  }

  // Follows `meta/active`: one load at a time, and an announcement that arrives while a load
  // runs gets one more load after it, so the page never settles on the older generation.
  function follower(load) {
    let busy = false;
    let asked = 0;
    return async function follow() {
      asked++;
      if (busy) return;
      busy = true;
      try {
        let seen;
        do {
          seen = asked;
          await load();
        } while (asked !== seen);
      } finally {
        busy = false;
      }
    };
  }

  function emptyData() {
    const d = {};
    for (const c of COLLECTIONS) d[c] = [];
    return d;
  }

  function repoOf(alias) {
    return String(alias || "").split("#")[0];
  }

  // A view (5.6): an empty include list means every repo, and exclude wins.
  function inScope(repo, view, groups) {
    if (!view) return true;
    const hits = (entries) =>
      (entries || []).some((e) => (e.repo ? e.repo === repo : ((groups || {})[e.group] || []).includes(repo)));
    if (hits(view.exclude)) return false;
    return !(view.include && view.include.length) || hits(view.include);
  }

  const isOpen = (i) => i.state === "open";
  const waiting = (i) => i.status === "needs-decision" || (i.status === "needs-review" && i.outside === true);
  const untriaged = (i) => i.status === "untriaged" || i.status === "new";

  // The summary tiles, with derive.rs's predicates so the page agrees with meta.summary.
  function tiles(issues) {
    const open = issues.filter(isOpen);
    return {
      p0_open: open.filter((i) => i.priority && i.priority.value === "p0").length,
      waiting_on_you: open.filter(waiting).length,
      untriaged: open.filter(untriaged).length,
      over_time_limit: open.filter((i) => i.limit_breached === true).length,
      open: open.length,
    };
  }

  // The Inbox: everything that needs the owner, oldest first. Phase 1 has no suggestions or
  // plans yet, so an untriaged issue is a card of its own, its main action setting a priority.
  function inbox(issues) {
    const cards = [];
    for (const i of issues.filter(isOpen)) {
      let why = null;
      if (i.status === "needs-decision") why = "decision";
      else if (i.status === "needs-review" && i.outside === true) why = "outside_pr";
      else if (untriaged(i)) why = "untriaged";
      if (why) cards.push({ why, issue: i });
    }
    return cards.sort((a, b) => cmp(a.issue.created_at, b.issue.created_at) || cmp(a.issue.alias, b.issue.alias));
  }

  // The Queue: open issues not in the Inbox, by level, then pull requests not in the Inbox.
  function queue(issues, inboxCards) {
    const shown = new Set(inboxCards.map((c) => c.issue.id));
    const rest = issues.filter((i) => isOpen(i) && !shown.has(i.id));
    const order = (a, b) =>
      cmp(a.rank == null ? Infinity : a.rank, b.rank == null ? Infinity : b.rank) ||
      (b.idle_days || 0) - (a.idle_days || 0) ||
      cmp(a.alias, b.alias);
    const levels = PRIORITIES.concat(["none"]).map((level) => ({
      level,
      items: rest
        .filter((i) => i.kind !== "pull_request" && ((i.priority && i.priority.value) || "none") === level)
        .sort(order),
    }));
    return { levels, pulls: rest.filter((i) => i.kind === "pull_request").sort(order) };
  }

  function cmp(a, b) {
    return a < b ? -1 : a > b ? 1 : 0;
  }

  // The oldest decision time whose ack the generation still carries; null with no snapshot.
  function ackHorizon(meta) {
    const t = Date.parse(meta && meta.taken_at);
    return Number.isNaN(t) ? null : new Date(t - ACK_DAYS * 86400000).toISOString().replace(/\.\d{3}Z$/, "Z");
  }

  // Which decisions a cycle has consumed (acks, 10.6) and which an undo cancelled. A decision
  // older than `horizon` with no ack is `settled`: its ack has aged out of the generation, and
  // the absence says nothing about whether a cycle consumed it. Returns
  // { byId: {id: {doc, ack, undoneBy, settled}}, latestFor(item) }.
  function decisionStates(decisions, acks, horizon) {
    const ackFor = new Map((acks || []).map((a) => [a.decision, a]));
    const byId = {};
    for (const d of decisions || []) {
      const ack = ackFor.get(d.id) || null;
      byId[d.id] = { doc: d, ack, undoneBy: null, settled: !ack && !!horizon && d.created_at < horizon };
    }
    for (const d of decisions || []) {
      if (d.action === "undo" && d.target && byId[d.target.decision]) {
        const t = byId[d.target.decision];
        if (!t.undoneBy || d.created_at < t.undoneBy.created_at) t.undoneBy = d;
      }
    }
    function latestFor(item) {
      let best = null;
      for (const s of Object.values(byId)) {
        const d = s.doc;
        if (d.action === "undo" || !d.target || d.target.item !== item || s.undoneBy || s.settled) continue;
        if (s.ack && s.ack.result !== "applied") continue;
        if (!best || d.created_at > best.doc.created_at || (d.created_at === best.doc.created_at && d.id > best.doc.id)) best = s;
      }
      return best;
    }
    return { byId, latestFor };
  }

  // When the tracker last synced, and each repo's read: a run that found the active
  // generation's content unchanged writes only `meta/heartbeat` (10.6), so the newer of the
  // two counts. A heartbeat for another generation (a slow run's, overtaken by a publish) or
  // older than the generation is ignored. Returns { at, repos }: meta.repos with each repo's
  // read_at and read from the heartbeat when it counts.
  function synced(meta, heartbeat) {
    if (!meta) return { at: null, repos: [] };
    const repos = meta.repos || [];
    const at = Date.parse(heartbeat && heartbeat.synced_at);
    const counts = !!heartbeat && heartbeat.generation === meta.generation && !Number.isNaN(at) &&
      (Number.isNaN(Date.parse(meta.taken_at)) || at > Date.parse(meta.taken_at));
    if (!counts) return { at: meta.taken_at, repos };
    const reads = new Map((heartbeat.repos || []).map((r) => [r.repo, r]));
    return {
      at: heartbeat.synced_at,
      repos: repos.map((r) => (reads.has(r.repo) ? Object.assign({}, r, { read_at: reads.get(r.repo).read_at, read: reads.get(r.repo).read }) : r)),
    };
  }

  // One repo's entry as an item row shows it: the generation's, with the read time and mode
  // of a heartbeat that counts (`synced`), so the row's read time is the latest run's. `stale`
  // and `error` stay the generation's: they are content, so a quiet run agrees with them.
  function syncedRepo(meta, heartbeat, repo) {
    return synced(meta, heartbeat).repos.find((r) => r.repo === repo);
  }

  // Minutes until the next sync run applies a decision, from the last sync's age (the newer
  // of the generation and `heartbeat`) and the configured interval; null when overdue.
  function nextRunMinutes(meta, now, heartbeat) {
    const interval = (meta && meta.sync_interval_minutes) || 5;
    const taken = Date.parse(synced(meta, heartbeat).at);
    if (Number.isNaN(taken)) return null;
    const left = interval - (now.getTime() - taken) / 60000;
    return left <= 0 ? null : Math.max(1, Math.ceil(left));
  }

  // Trends over the last `weeks` whole weeks ending at `now`: opened and closed per week, the
  // open items over their time limit ("N of M"), and the same per group and per repo.
  function trends(issues, now, weeks, groups) {
    const week = 7 * 24 * 3600 * 1000;
    const end = now.getTime();
    const buckets = [];
    for (let k = weeks - 1; k >= 0; k--) {
      const from = end - (k + 1) * week;
      const to = end - k * week;
      const within = (t) => {
        const x = Date.parse(t);
        return x > from && x <= to;
      };
      buckets.push({
        from: new Date(from).toISOString(),
        opened: issues.filter((i) => within(i.created_at)).length,
        closed: issues.filter((i) => i.closed_at && within(i.closed_at)).length,
      });
    }
    const open = issues.filter(isOpen);
    const over = (list) => ({ n: list.filter((i) => i.limit_breached === true).length, of: list.length });
    const byRepo = {};
    for (const i of open) (byRepo[repoOf(i.alias)] = byRepo[repoOf(i.alias)] || []).push(i);
    const byGroup = {};
    for (const [g, repos] of Object.entries(groups || {})) byGroup[g] = open.filter((i) => repos.includes(repoOf(i.alias)));
    const summarise = (m) =>
      Object.fromEntries(Object.entries(m).map(([k, list]) => [k, Object.assign(over(list), tiles(list))]));
    return { weeks: buckets, over: over(open), byRepo: summarise(byRepo), byGroup: summarise(byGroup) };
  }

  // The launch state a row shows: the tracker's launch_status, else `not_sent` when the page
  // recorded a refused send, else `requested`.
  function launchStates(launches, statuses) {
    const notSent = new Map();
    for (const l of launches || []) if (l.not_sent) notSent.set(l.not_sent.launch, l.not_sent.error);
    const status = new Map((statuses || []).map((s) => [s.launch, s]));
    const byItem = {};
    for (const l of launches || []) {
      if (!Array.isArray(l.items)) continue;
      const s = status.get(l.id);
      // The tracker reports every launch it ingested as `requested`, heard or not: only a later
      // transition outranks the page's record of a refused send.
      const state = s && !(s.state === "requested" && notSent.has(l.id)) ? s.state : notSent.has(l.id) ? "not_sent" : "requested";
      const entry = { launch: l, state, error: notSent.get(l.id) || null };
      for (const item of l.items) {
        const cur = byItem[item];
        if (!cur || l.created_at > cur.launch.created_at) byItem[item] = entry;
      }
    }
    return byItem;
  }

  const pad = (n, w) => String(n).padStart(w || 2, "0");

  // A page document id (5.3): <prefix>_<yyyymmdd>_<hhmmss>_<8 random>, letters, digits and _.
  // There is no create-only write (db.d.ts), so the randomness is what keeps two writes in one
  // second apart: 36^8 values.
  function newId(prefix, now, rand) {
    const r = Math.min(36 ** 8 - 1, Math.floor((rand || Math.random)() * 36 ** 8)).toString(36).padStart(8, "0");
    return (
      prefix + "_" +
      now.getUTCFullYear() + pad(now.getUTCMonth() + 1) + pad(now.getUTCDate()) + "_" +
      pad(now.getUTCHours()) + pad(now.getUTCMinutes()) + pad(now.getUTCSeconds()) + "_" + r
    );
  }

  function stamp(now) {
    return now.toISOString().replace(/\.\d{3}Z$/, "Z");
  }

  // A decisions document (page-decision.schema.json). `fields` carries action, target and
  // the optional value and reason.
  function decisionDoc(fields, who, now, rand) {
    const doc = {
      id: newId("D", now, rand),
      project: who.project,
      author: who.author,
      created_at: stamp(now),
      action: fields.action,
      target: fields.target,
    };
    if (fields.value) doc.value = fields.value;
    if (fields.reason) doc.reason = fields.reason;
    return doc;
  }

  function launchDoc(fields, who, now, rand) {
    return {
      id: newId("L", now, rand),
      project: who.project,
      author: who.author,
      created_at: stamp(now),
      kind: fields.kind,
      items: fields.items,
      aliases: fields.aliases,
      recipe: fields.recipe,
      routing: fields.routing,
    };
  }

  function notSentDoc(launchId, error, who, now, rand) {
    return {
      id: newId("L", now, rand),
      project: who.project,
      author: who.author,
      created_at: stamp(now),
      not_sent: { launch: launchId, error: String(error || "upstream_error") },
    };
  }

  function viewDoc(name, view, who, now, rand) {
    return {
      id: newId("V", now, rand),
      project: who.project,
      author: who.author,
      created_at: stamp(now),
      name,
      include: view.include || [],
      exclude: view.exclude || [],
    };
  }

  function requestDoc(fields, who, now, rand) {
    const doc = { id: newId("R", now, rand), project: who.project, author: who.author, created_at: stamp(now), kind: fields.kind };
    if (fields.items) doc.items = fields.items;
    if (fields.launch) doc.launch = fields.launch;
    return doc;
  }

  // A sync request (7.2, 10.8): for one item, or with no item for the whole project. In phase 1
  // the page cannot wake the sync job, so the next scheduled run serves it; the note says when.
  function syncFields(item) {
    return item ? { kind: "sync", items: [item.id] } : { kind: "sync" };
  }
  function syncNote(item, minutes) {
    const when = minutes ? "within " + minutes + " minute" + (minutes === 1 ? "" : "s") : "at the next sync";
    return (item ? "Refresh of " + item.alias : "Sync") + " requested: the next run reads it " + when + ".";
  }

  // A thread record (5.4): one per launch thread, written once when the thread starts.
  function threadDoc(thread, who, now, rand) {
    return { id: newId("T", now, rand), project: who.project, author: who.author, created_at: stamp(now), thread };
  }

  // The launch thread: the newest thread record in `launches`. The first slice kept one record
  // at `launches/_thread` and rewrote it; it still counts, as the oldest.
  async function currentThread(db) {
    const q = await db.collection("launches").limit(LIST_LIMIT).get();
    let best = null;
    for (const snap of q.docs) {
      const d = snap.data() || {};
      if (typeof d.thread !== "string") continue;
      const at = d.created_at || "";
      if (!best || at > best.at || (at === best.at && d.id > best.id)) best = { at, id: d.id, thread: d.thread };
    }
    return best && best.thread;
  }

  // Launch (5.4): the gate, the current thread, the launches document, then a comment holding
  // only its id into that thread. A refused send writes a not_sent document naming the launch.
  // A thread refused as gone or full starts a new one, which gets its own thread record.
  async function launch(deps, fields) {
    const gate = await deps.comments.canSendToClaude().catch(() => "unavailable");
    if (gate !== "available") return { ok: false, gate };
    // Read before anything is written: a launch document is always followed by a send or a
    // not_sent.
    let thread;
    try {
      thread = await currentThread(deps.db);
    } catch (e) {
      return { ok: false, error: (e && e.code) || "unavailable" };
    }
    const doc = launchDoc(fields, deps.who, deps.now(), deps.rand);
    await deps.db.doc("launches/" + doc.id).set(doc);
    let sent;
    try {
      try {
        if (!thread) throw { code: "not_found" };
        sent = await deps.comments.sendToClaude({ threadId: thread, text: doc.id });
      } catch (e) {
        // A full thread comes back as upstream_error (comments.d.ts): the one retry is a
        // new thread.
        if (!e || (e.code !== "not_found" && e.code !== "upstream_error")) throw e;
        sent = await deps.comments.sendToClaude({ anchor: await deps.anchor(), text: doc.id });
      }
    } catch (e) {
      const code = (e && e.code) || "upstream_error";
      const refused = notSentDoc(doc.id, code, deps.who, deps.now(), deps.rand);
      await deps.db.doc("launches/" + refused.id).set(refused);
      return { ok: false, launch: doc, error: code };
    }
    if (sent.threadId && sent.threadId !== thread) {
      // The launch was heard; a record that fails only means the next launch starts a thread.
      const t = threadDoc(sent.threadId, deps.who, deps.now(), deps.rand);
      try {
        await deps.db.doc("launches/" + t.id).set(t);
      } catch (e) {
        return { ok: true, launch: doc, threadId: sent.threadId, threadNotRecorded: (e && e.code) || "error" };
      }
    }
    return { ok: true, launch: doc, threadId: sent.threadId };
  }

  // --- closing (5.2a, 10.10) -------------------------------------------------------------

  const CLOSE_REASONS = { completed: "completed", not_planned: "not planned", duplicate: "a duplicate" };
  const COMMENT_MAX = 65536;

  // Close is offered only on an open issue in a repo the meta names writable.
  function closable(issue, repo) {
    return issue.kind === "issue" && issue.state === "open" && !!repo && !(repo.read_only && repo.read_only.value);
  }

  function closeComment(reason, duplicateOf) {
    if (reason === "duplicate") return "Closed from the issue dashboard as a duplicate of #" + (duplicateOf || "N") + ".";
    if (reason === "not_planned") return "Closed from the issue dashboard: no longer relevant.";
    return "Closed from the issue dashboard: done.";
  }

  // The close sheet's fields as a decision's fields, or { error } saying what to fix.
  function closeFields(issue, reason, duplicateOf, comment) {
    if (!CLOSE_REASONS[reason]) return { error: "Choose a reason." };
    const close = { reason, comment: String(comment || "").trim() };
    if (close.comment.length > COMMENT_MAX) return { error: "The comment is too long for GitHub." };
    if (reason === "duplicate") {
      const m = /^#?\s*(\d+)$/.exec(String(duplicateOf || "").trim());
      const n = m ? Number(m[1]) : 0;
      if (!Number.isSafeInteger(n) || n < 1) return { error: "Give the number of the issue this duplicates." };
      if (String(issue.alias).endsWith("#" + n)) return { error: "An issue cannot duplicate itself." };
      close.duplicate_of = n;
    }
    return { fields: { action: "close", target: { item: issue.id, alias: issue.alias }, value: { close } } };
  }

  function describeClose(c) {
    return "close as " + (c.reason === "duplicate" ? "a duplicate of #" + c.duplicate_of : CLOSE_REASONS[c.reason] || c.reason);
  }

  // The item's close decision no cycle has acked and no undo took back, if any.
  function pendingClose(decisions, acks, item, horizon) {
    const st = decisionStates(decisions, acks, horizon);
    const open = Object.values(st.byId).filter((s) => s.doc.action === "close" && s.doc.target && s.doc.target.item === item && !s.ack && !s.undoneBy && !s.settled);
    open.sort((a, b) => cmp(b.doc.created_at, a.doc.created_at));
    return open.length ? open[0].doc : null;
  }

  // Whether the owner dismissed this closed item (5.8): the page holds a dismiss of this very
  // close still in force, or, when it holds none, the tracker says so (dismissed_at). The page's
  // own decisions win so a Dismiss or its Undo shows at once.
  function dismissed(issue, decisions, acks) {
    if (issue.state === "open" || !issue.closed_at) return false;
    const mine = (decisions || []).filter((d) =>
      d.action === "dismiss_closed" && d.target && d.target.item === issue.id && d.value && Date.parse(d.value.closed_at) === Date.parse(issue.closed_at));
    if (!mine.length) return Boolean(issue.dismissed_at);
    const result = new Map((acks || []).map((a) => [a.decision, a.result]));
    const undos = new Map();
    for (const d of decisions || []) {
      if (d.action !== "undo" || !d.target || !d.target.decision) continue;
      if (!undos.has(d.target.decision)) undos.set(d.target.decision, []);
      undos.get(d.target.decision).push(d.id);
    }
    // In force: not refused, and not cancelled by an undo in force, along a chain of undos as
    // decision.rs resolves it. With no undo of it on the page (one past the list's limit), a
    // `cancelled` ack is the tracker's word.
    const visiting = new Set();
    const inForce = (id) => {
      if (visiting.has(id)) return true;
      if (result.get(id) === "refused") return false;
      const by = undos.get(id) || [];
      if (!by.length) return result.get(id) !== "cancelled";
      visiting.add(id);
      const cancelled = by.some(inForce);
      visiting.delete(id);
      return !cancelled;
    };
    return mine.some((d) => inForce(d.id));
  }

  const CLOSED_AS = { completed: "completed", not_planned: "not planned", duplicate: "a duplicate" };

  // Recently closed (5.8): every issue closed in the last `days` days that the owner has not
  // dismissed, newest first: { issue, at, close, reopening, reason, prs, dismissible }. `close` is
  // the dashboard's own close, whose Reopen stays while Undo is allowed (`reopening`: an unacked
  // undo of it); any other close can be dismissed.
  function recentlyClosed(issues, decisions, acks, now, days) {
    const since = now.getTime() - days * 86400000;
    const acked = new Set((acks || []).map((a) => a.decision));
    const out = [];
    for (const i of issues) {
      const c = i.dashboard_close || null;
      const at = i.closed_at || (c && c.at);
      if (i.kind !== "issue" || (i.state === "open" && !c) || !(Date.parse(at) >= since) || dismissed(i, decisions, acks)) continue;
      const reopening = c ? (decisions || []).find((d) => d.action === "undo" && d.target && d.target.decision === c.decision && !acked.has(d.id)) || null : null;
      out.push({
        issue: i,
        at,
        close: c,
        reopening,
        reason: CLOSED_AS[(c && c.reason) || i.state_reason] || null,
        prs: i.closed_by || [],
        dismissible: i.state !== "open" && !!i.closed_at && !(c && (c.undo_allowed || reopening)),
      });
    }
    return out.sort((a, b) => cmp(b.at, a.at) || cmp(a.issue.alias, b.issue.alias));
  }

  // A Dismiss of the close the page shows: bound to its closed_at, so a reopen lapses it (5.8).
  function dismissFields(issue) {
    return { action: "dismiss_closed", target: { item: issue.id, alias: issue.alias }, value: { closed_at: issue.closed_at } };
  }

  // The app's sessions on an item as the page says them (10.10): running or idle, how long since
  // it was last active, and whether the match is only probable. Newest first, as derived.
  function sessionLines(issue, now) {
    return (issue.sessions || []).map((s) => {
      const mins = Math.max(0, Math.floor((now.getTime() - Date.parse(s.last_active_at)) / 60000));
      const ago = mins < 1 ? "just now" : mins < 60 ? mins + " min ago" : mins < 1440 ? Math.floor(mins / 60) + " h ago" : Math.floor(mins / 1440) + " d ago";
      return {
        id: s.id,
        title: s.title || "Untitled session",
        state: s.running ? "running" : "idle",
        ago,
        probable: Boolean(s.match && s.match.kind === "probable"),
        link: s.link || null,
      };
    });
  }

  // The Queue's "In progress" filter: a session is working on it, or its status says so.
  function inProgress(issue) {
    return (issue.sessions || []).length > 0 || issue.status === "in-progress";
  }

  // What to tell the artifact's owner when the tracker would refuse their documents (10.5): the
  // config's owner_id is still pending (null in meta), or is another page user's. A generation
  // from before meta carried owner_id (the key absent) says nothing.
  function ownerNotice(meta, viewer, isOwner) {
    if (!meta || !isOwner || !viewer || !("owner_id" in meta) || meta.owner_id === viewer) return null;
    const out = { id: viewer, line: 'owner_id = "' + viewer + '"', file: "projects/" + meta.project + ".toml" };
    if (meta.owner_id == null) return Object.assign({ kind: "pending" }, out);
    return Object.assign({ kind: "mismatch", configured: meta.owner_id }, out);
  }

  // --- closing recommendations (5.8) ----------------------------------------------------

  // The page's dismissals still in force: not refused, not cancelled, and not undone.
  // The suggestions the page hides before a cycle has seen their dismissal. Once a cycle has
  // consumed it (any ack), the tracker's documents are the word: it leaves an applied one out
  // until the issue changes, then shows it again under the same id.
  function dismissedSuggestions(decisions, acks, horizon) {
    const st = decisionStates(decisions, acks, horizon);
    const out = new Set();
    for (const s of Object.values(st.byId)) {
      const d = s.doc;
      if (d.action !== "dismiss_suggestion" || !d.target || !d.target.suggestion) continue;
      if (s.ack || s.undoneBy || s.settled) continue;
      out.add(d.target.suggestion);
    }
    return out;
  }

  // The recommendations the Inbox shows for `issues` (the list in view): each card's issue (a
  // batch's first member), its open members, and the issues a run left open with why. A
  // suggestion the page has dismissed leaves at once; one about an issue not in view, or
  // closed since, is not shown; a batch with fewer than two members still open is not either.
  function suggestionCards(suggestions, issues, decisions, acks, horizon) {
    const byId = new Map(issues.filter(isOpen).map((i) => [i.id, i]));
    const gone = dismissedSuggestions(decisions, acks, horizon);
    const cards = [];
    const leftOut = [];
    for (const s of suggestions || []) {
      if (gone.has(s.id)) continue;
      if (s.kind === "batch") {
        const members = (s.members || []).map((m) => byId.get(m)).filter(Boolean);
        if (members.length < 2) continue;
        cards.push({ s, issue: members[0], members });
        continue;
      }
      const issue = byId.get(s.item);
      if (!issue) continue;
      if (s.kind === "left_out") leftOut.push({ s, issue });
      else if (["likely_fixed", "duplicate", "obsolete"].includes(s.kind)) cards.push({ s, issue, members: [issue] });
    }
    const order = (a, b) => cmp(a.s.made_at, b.s.made_at) || cmp(a.issue.alias, b.issue.alias);
    return { cards: cards.sort(order), leftOut: leftOut.sort(order) };
  }

  function dismissSuggestionFields(s) {
    return { action: "dismiss_suggestion", target: { suggestion: s.id, item: s.item, alias: s.alias } };
  }

  // What the close sheet opens with for a suggestion: the owner's Close is the approval.
  // A duplicate in another repo cannot be closed as one by number, so it closes as not planned
  // with the other issue named in the comment.
  function closePrefill(s) {
    if (s.kind === "likely_fixed") return { reason: "completed", comment: "Closed from the issue dashboard: fixed by " + s.commit + "." };
    if (s.kind === "obsolete") return { reason: "not_planned" };
    if (s.kind === "duplicate") {
      const other = String(s.related_alias || "");
      if (other && repoOf(other) === repoOf(s.alias)) return { reason: "duplicate", duplicateOf: other.split("#")[1] };
      return { reason: "not_planned", comment: "Closed from the issue dashboard as a duplicate of " + (other || "another issue") + "." };
    }
    return null;
  }

  // A launch of a fix (one issue) or a batch (several of one repo) through its repo's recipe
  // (7.6), or { error } when the repo has none or the issues span repos.
  function fixLaunchFields(issues, repo, routing) {
    if (!issues.length) return { error: "Nothing to launch." };
    if (new Set(issues.map((i) => repoOf(i.alias))).size !== 1) return { error: "A batch is one repo's issues." };
    if (!repo || !repo.recipe) return { error: repoOf(issues[0].alias) + " has no launch recipe in the tracker's config." };
    return {
      fields: {
        kind: issues.length > 1 ? "batch" : "fix",
        items: issues.map((i) => i.id),
        aliases: issues.map((i) => i.alias),
        recipe: repo.recipe,
        routing,
      },
    };
  }

  const READ_ONLY_RECIPE = "read-only";

  // Recommend closures (5.8) over the issues in view: every open issue of a writable repo.
  function recommendFields(issues, repoFor, routing) {
    const items = issues.filter((i) => closable(i, repoFor(repoOf(i.alias))));
    if (!items.length) return { error: "No open issue in view can be closed from here." };
    return {
      fields: {
        kind: "recommend_closures",
        items: items.map((i) => i.id),
        aliases: items.map((i) => i.alias),
        recipe: READ_ONLY_RECIPE,
        routing,
      },
    };
  }

  const api = {
    PRIORITIES, COLLECTIONS, LIST_LIMIT, CLOSE_REASONS, ACK_DAYS,
    loadGeneration, follower, ackHorizon, threadDoc, repoOf, inScope, tiles, inbox, queue, decisionStates, synced, syncedRepo, nextRunMinutes,
    trends, launchStates, newId, decisionDoc, launchDoc, notSentDoc, viewDoc, requestDoc, syncFields, syncNote, launch,
    closable, closeComment, closeFields, describeClose, pendingClose, dismissed, recentlyClosed, dismissFields,
    sessionLines, inProgress, ownerNotice,
    dismissedSuggestions, suggestionCards, dismissSuggestionFields, closePrefill, fixLaunchFields, recommendFields, READ_ONLY_RECIPE,
  };
  root.IssueDashboard = api;
  if (typeof module === "object" && module.exports) module.exports = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
