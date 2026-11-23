// The page's script: the working magictree screen in the hero, the branch
// demo, the command tabs, the copy button and the reveals. Everything the page
// says is in the HTML; without this file the hero shows a picture of the
// screen and every command group is listed.

const esc = (text) => text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const pad = (text, width) => text + " ".repeat(Math.max(0, width - text.length));
const span = (cls, text) => `<span class="${cls}">${esc(text)}</span>`;

// ---------- reveals ----------

(() => {
  const targets = document.querySelectorAll("[data-reveal], .shutter");
  if (!("IntersectionObserver" in window)) {
    targets.forEach((el) => el.classList.add("is-in"));
    return;
  }
  const io = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.classList.add("is-in");
        io.unobserve(entry.target);
      }
    },
    { rootMargin: "0px 0px -8% 0px", threshold: 0.05 },
  );
  targets.forEach((el) => io.observe(el));
  // Nothing stays hidden because an observer never fired.
  window.addEventListener("load", () =>
    setTimeout(() => {
      targets.forEach((el) => {
        const box = el.getBoundingClientRect();
        if (box.top < innerHeight && box.bottom > 0) el.classList.add("is-in");
      });
    }, 2500),
  );
})();

// ---------- the screen ----------

(() => {
  const root = document.getElementById("tui");
  if (!root) return;

  const SECTIONS = ["services", "worktrees", "ports", "env", "logs"];
  const KEYS = {
    services: [["l", "logs"], ["u", "up"], ["s", "start"], ["r", "restart"], ["d", "down"], ["o", "open"], [":", "command"], ["q", "quit"]],
    worktrees: [["enter", "open"], ["x", "remove"], ["c", "gc"], ["r", "refresh"], [":", "command"], ["q", "quit"]],
    ports: [["↑↓", "move"], ["u", "up"], ["d", "down"], ["esc", "services"], [":", "command"], ["q", "quit"]],
    env: [["↑↓", "move"], ["u", "up"], ["d", "down"], ["esc", "services"], [":", "command"], ["q", "quit"]],
    logs: [["↑↓", "scroll"], ["g/G", "top/follow"], ["←→", "pan"], [":", "command"], ["esc", "back"]],
  };

  const state = {
    section: "services",
    row: { services: 4, worktrees: 1, ports: 2, env: 0 },
    note: "",
    pane: null,
    services: [
      { name: "db", runtime: "compose", state: "running (healthy)", port: 24100 },
      { name: "redis", runtime: "compose", state: "running (healthy)", port: 24103 },
      { name: "mailpit", runtime: "compose", state: "exited", port: 24104 },
      { name: "api", runtime: "host", state: "running (pid 48190)", port: 24101 },
      { name: "web", runtime: "host", state: "running (pid 48211)", port: 24102, alias: true },
      { name: "worker", runtime: "host", state: "stopped", port: null },
    ],
    log: [],
  };

  const url = (service) =>
    service.port === null
      ? ""
      : service.alias
        ? `http://shop-feat-billing.localhost:${service.port}`
        : `http://localhost:${service.port}`;

  const tone = (text) =>
    /unhealthy|exited|dead/.test(text) ? "red" : /starting|restarting/.test(text) ? "yellow" : /^running/.test(text) ? "green" : "dim";

  const worktrees = [
    ["shop", "main", "primary", "db=5432 api=8000 web=5173", "~/code/shop"],
    ["shop", "shop-feat-billing", "live", "db=24100 api=24101 web=24102", "~/code/shop-feat-billing"],
    ["shop", "shop-fix-checkout", "live", "db=27360 api=27361 web=27362", "~/code/shop-fix-checkout"],
    ["shop", "shop-spike-search", "gone", "db=21480 api=21481 web=21482", "~/code/shop-spike-search"],
    ["notes", "main", "primary", "web=3000", "~/code/notes"],
  ];

  const env = () => [
    ["API_PORT", "24101"],
    ["COMPOSE_PROJECT_NAME", "shop-feat-billing"],
    ["DATABASE_URL", "postgres://localhost:24100/shop"],
    ["MAGICTREE_PORT_api", "24101"],
    ["MAGICTREE_PORT_db", "24100"],
    ["MAGICTREE_PORT_web", "24102"],
    ["MAGICTREE_SLUG", "shop-feat-billing"],
    ["MAGICTREE_WORKTREE", "/Users/you/code/shop-feat-billing"],
    ["PORT", "24102"],
    ["PUBLIC_URL", "http://shop-feat-billing.localhost:24102"],
  ];

  const LOG_LINES = [
    "GET /api/invoices 200 in 12ms",
    "GET /assets/app.css 304 in 1ms",
    "POST /api/invoices 201 in 48ms",
    "[vite] hmr update /src/billing/Invoice.tsx",
    "GET /api/customers/42 200 in 9ms",
    "GET / 200 in 4ms",
    "[vite] page reload src/routes.ts",
    "GET /api/invoices?page=2 200 in 15ms",
  ];
  const stamp = () => new Date().toTimeString().slice(0, 8);
  for (let i = 0; i < 9; i++) state.log.push(`${stamp()} ${LOG_LINES[i % LOG_LINES.length]}`);

  function rows() {
    if (state.section === "services") {
      const head = `${pad("SERVICE", 9)}${pad("RUNTIME", 9)}${pad("STATE", 21)}${pad("PORT", 7)}URL`;
      const body = state.services.map(
        (s) =>
          `${pad(s.name, 9)}${pad(s.runtime, 9)}${span(tone(s.state), pad(s.state, 21))}${pad(s.port === null ? "" : String(s.port), 7)}${esc(url(s))}`,
      );
      return { head, body };
    }
    if (state.section === "worktrees") {
      const head = `${pad("REPOSITORY", 12)}${pad("WORKTREE", 19)}${pad("STATE", 9)}${pad("PORTS", 30)}PATH`;
      const body = worktrees.map(
        ([repo, id, live, ports, path]) =>
          `${pad(repo, 12)}${pad(id, 19)}${span(live === "gone" ? "red" : "green", pad(live, 9))}${pad(ports, 30)}${path}`,
      );
      return { head, body };
    }
    if (state.section === "ports") {
      const head = `${pad("NAME", 8)}${pad("PORT", 7)}URL`;
      const body = state.services.filter((s) => s.port !== null).map((s) => `${pad(s.name, 8)}${pad(String(s.port), 7)}${esc(url(s))}`);
      return { head, body };
    }
    if (state.section === "env") {
      const head = `${pad("VARIABLE", 22)}VALUE`;
      return { head, body: env().map(([key, value]) => `${pad(key, 22)}${esc(value)}`) };
    }
    return { head: null, body: state.log.slice(-10).map(esc) };
  }

  function render() {
    const keys = KEYS[state.section]
      .map(([key, what]) => `<span>${span("lime", `<${key}>`)} ${span("dim", what)}</span>`)
      .join("");
    const { head, body } = rows();
    const selected = state.row[state.section];
    const title =
      state.section === "logs" ? "logs · web · following" : state.section;
    const list = body
      .map((line, index) =>
        state.section === "logs"
          ? `<span class="tui__row" style="cursor:default">${line}</span>`
          : `<button class="tui__row" type="button" role="option" data-row="${index}" aria-selected="${index === selected}">${line}</button>`,
      )
      .join("");
    const place =
      state.section === "worktrees"
        ? `<b>magictree 0.2.0</b>\n${span("dim", "per-worktree development stacks")}\n`
        : `<b>magictree 0.2.0</b>\n<b>shop · shop-feat-billing</b>\n${span("dim", "~/code/shop-feat-billing")}`;
    const pane = state.pane
      ? `<span class="tui__pane"><span class="tui__title">magictree ${esc(state.pane.label)} ${span(state.pane.done ? "green" : "yellow", state.pane.done ? "done" : "running…")}</span>${state.pane.lines.map(esc).join("\n")}</span>`
      : "";
    root.innerHTML =
      `<span class="tui__head"><span class="tui__mark">▄▄▄▄▄\n█ █ █\n<span class="mk">▀</span> <span class="mk">▀</span> <span class="mk">▀</span></span><span class="tui__place">${place}</span><span class="tui__keys">${keys}</span></span>` +
      `<span class="tui__tabs" role="tablist" aria-label="Sections">${SECTIONS.map(
        (name, index) =>
          `<button class="tui__tab" type="button" role="tab" data-section="${name}" aria-selected="${name === state.section}">${index + 1} ${name}</button>`,
      ).join("")}</span>` +
      `<span class="tui__panel"><span class="tui__title">${esc(title)}</span><span class="tui__rows" role="listbox" aria-label="${esc(state.section)}">${head ? `<span class="tui__row tui__row--head">${head}</span>` : ""}${list}</span></span>` +
      pane +
      `<span class="tui__note">${esc(state.note)}</span>`;
  }

  function open(section) {
    state.section = section;
    state.note = "";
    render();
  }

  function move(delta) {
    if (state.section === "logs") return;
    const count = rows().body.length;
    state.row[state.section] = Math.max(0, Math.min(count - 1, state.row[state.section] + delta));
    render();
  }

  // What a key does here is what it does in the real screen, played out on
  // the sample stack: the command it runs, and the state that follows.
  let running = false;
  function play(label, steps) {
    if (running) {
      state.note = `\`${state.pane.label}\` is still running`;
      return render();
    }
    running = true;
    state.pane = { label, lines: [], done: false };
    state.note = "";
    render();
    steps.forEach(([line, change], index) => {
      setTimeout(() => {
        if (line) state.pane.lines.push(line);
        if (change) change();
        if (index === steps.length - 1) {
          state.pane.done = true;
          running = false;
        }
        render();
      }, 450 * (index + 1));
    });
  }

  const pids = { api: 48190, web: 48211, worker: 48236 };
  function act(key) {
    const service = state.services[state.row.services];
    const stack = ["services", "ports", "env"].includes(state.section);
    if (key === "u" && stack) {
      return play("up", [
        ["[1/2] service · worker"],
        ["     worker: started (pid 48236)", () => (state.services[5].state = "running (pid 48236)")],
        ["     healthy"],
      ]);
    }
    if (key === "d" && stack) {
      return play("down", [
        ["worker: stopped", () => (state.services[5].state = "stopped")],
        ["web: stopped", () => (state.services[4].state = "stopped")],
        ["api: stopped", () => (state.services[3].state = "stopped")],
        ["compose: db, redis stopped", () => state.services.slice(0, 2).forEach((s) => (s.state = "exited"))],
      ]);
    }
    if (state.section !== "services") return;
    if (key === "r" || key === "s") {
      const host = service.runtime === "host";
      const label = `${key === "r" ? "restart" : "up"} ${service.name}`;
      const up = host ? `running (pid ${pids[service.name] + 40})` : "running (healthy)";
      return play(label, [
        [`${service.name}: ${host ? "starting" : "restarting container"}`, () => (service.state = "starting")],
        [`${service.name}: ${host ? `started (pid ${pids[service.name] + 40})` : "starting → healthy"}`, () => (service.state = up)],
        ["     healthy"],
      ]);
    }
    if (key === "o") {
      state.note = service.port === null ? `${service.name} has no port` : `opened ${url(service)}`;
      return render();
    }
    if (key === "l" || key === "Enter") return open("logs");
  }

  root.addEventListener("click", (event) => {
    const tab = event.target.closest("[data-section]");
    if (tab) return open(tab.dataset.section);
    const row = event.target.closest("[data-row]");
    if (row) {
      state.row[state.section] = Number(row.dataset.row);
      render();
    }
  });

  root.addEventListener("keydown", (event) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const key = event.key;
    if (/^[1-5]$/.test(key)) return open(SECTIONS[Number(key) - 1]);
    if (key === "ArrowDown" || key === "j") {
      event.preventDefault();
      return move(1);
    }
    if (key === "ArrowUp" || key === "k") {
      event.preventDefault();
      return move(-1);
    }
    if (key === "Escape" && state.section !== "services") return open("services");
    if (["u", "d", "r", "s", "o", "l", "Enter"].includes(key)) {
      if (key === "Enter" && event.target.closest("[data-section]")) return;
      event.preventDefault();
      act(key);
    }
  });

  // The log section follows a service that is still writing.
  if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
    let n = 9;
    setInterval(() => {
      if (document.hidden) return;
      state.log.push(`${stamp()} ${LOG_LINES[n++ % LOG_LINES.length]}`);
      if (state.log.length > 40) state.log.shift();
      if (state.section === "logs") render();
    }, 1400);
  }

  render();
})();

// ---------- name a branch, get a stack ----------

(() => {
  const out = document.getElementById("demo-out");
  const form = document.getElementById("demo-controls");
  if (!out || !form) return;
  const branch = document.getElementById("demo-branch");
  const alias = document.getElementById("demo-alias");
  const primary = document.getElementById("demo-primary");

  // magictree's own rule for a slug: lowercase, runs of anything else become
  // one dash, at most 32 characters.
  const slugify = (text) => {
    let slug = text
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 32)
      .replace(/-+$/, "");
    return slug || "worktree";
  };

  // A stand-in for the hash that picks a block: the same name always lands on
  // the same one of the 638 blocks between 20000 and 32767.
  const block = (name) => {
    let hash = 2166136261;
    for (const ch of name) hash = Math.imul(hash ^ ch.codePointAt(0), 16777619) >>> 0;
    return 20000 + (hash % 638) * 20;
  };

  const DECLARED = { db: 5432, redis: 6379, api: 8000, web: 5173 };
  const ORDER = ["db", "redis", "api", "web"];

  function render() {
    const name = branch.value.trim() || "feat/billing";
    const isPrimary = primary.checked;
    const dir = isPrimary ? "shop" : `shop-${name.replace(/[^A-Za-z0-9._-]+/g, "-")}`;
    const slug = isPrimary ? "main" : slugify(dir);
    const start = block(dir);
    const chosen = ORDER.filter((id) => form.querySelector(`[data-service="${id}"]`).checked);
    const lines = [];
    if (isPrimary) {
      lines.push(`${span("dim", "$")} cd ~/code/shop &amp;&amp; magictree ports`);
      lines.push(`worktree main (declared ports)`);
    } else {
      lines.push(`${span("dim", "$")} magictree new ${esc(name)} --no-up`);
      lines.push(`${span("dim", "$")} cd ../${esc(dir)} &amp;&amp; magictree ports`);
      lines.push(`worktree ${esc(slug)} (generated ports, block ${start}-${start + 19})`);
    }
    if (!chosen.length) lines.push(span("dim", "no service exposes a port"));
    chosen.forEach((id, index) => {
      const port = isPrimary ? DECLARED[id] : start + index;
      const host = id === "web" && alias.checked ? `${slug}.localhost` : "localhost";
      lines.push(`${pad(id, 8)} ${pad(String(port), 6)} ${span("lime", `http://${host}:${port}`)}`);
    });
    lines.push("");
    lines.push(`${span("dim", "$")} magictree env`);
    lines.push(`MAGICTREE_SLUG=${esc(slug)}`);
    chosen.forEach((id, index) => lines.push(`MAGICTREE_PORT_${id}=${isPrimary ? DECLARED[id] : start + index}`));
    if (chosen.includes("web")) {
      const port = isPrimary ? DECLARED.web : start + chosen.indexOf("web");
      lines.push(`PUBLIC_URL=http://${alias.checked ? `${slug}.localhost` : "localhost"}:${port}`);
    }
    out.innerHTML = lines.join("\n");
    branch.disabled = isPrimary;
  }

  form.addEventListener("input", render);
  render();
})();

// ---------- command groups ----------

(() => {
  const tabs = [...document.querySelectorAll(".cmds__tab")];
  if (!tabs.length) return;
  const select = (tab, focus) => {
    tabs.forEach((other) => {
      const on = other === tab;
      other.setAttribute("aria-selected", String(on));
      other.tabIndex = on ? 0 : -1;
      document.getElementById(other.getAttribute("aria-controls")).hidden = !on;
    });
    if (focus) tab.focus();
  };
  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => select(tab, false));
    tab.addEventListener("keydown", (event) => {
      const step = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 }[event.key];
      if (!step) return;
      event.preventDefault();
      select(tabs[(index + step + tabs.length) % tabs.length], true);
    });
  });
})();

// ---------- copy the install command ----------

(() => {
  const button = document.getElementById("install-copy");
  const command = document.getElementById("install-cmd");
  if (!button || !command) return;
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(command.textContent.trim());
      button.textContent = "copied";
    } catch {
      const range = document.createRange();
      range.selectNodeContents(command);
      getSelection().removeAllRanges();
      getSelection().addRange(range);
      button.textContent = "selected";
    }
    setTimeout(() => (button.textContent = "copy"), 1800);
  });
})();
