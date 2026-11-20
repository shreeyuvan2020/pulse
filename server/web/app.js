"use strict";

const topBar = document.getElementById("top");
const view = document.getElementById("view");
const SVG = "http://www.w3.org/2000/svg";

// every value from the server goes in as text, never as HTML
function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "checked") node.checked = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (value != null && value !== false) node.setAttribute(key, value === true ? "" : value);
  }
  node.append(...children.flat().filter((child) => child != null && child !== false));
  return node;
}

async function api(path, { method = "GET", body } = {}) {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body ? { "content-type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  if (response.status === 204) return null;
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw Object.assign(new Error(data.error || response.statusText), { status: response.status });
  return data;
}

function duration(seconds) {
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, "0")}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${seconds}s`;
}

function ago(unixSeconds) {
  const seconds = Math.max(0, Math.floor(Date.now() / 1000 - unixSeconds));
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function renderTop(me) {
  const nav = el("nav");
  if (me.user) {
    nav.append(
      el("a", { href: `/u/${encodeURIComponent(me.user.username)}` }, `@${me.user.username}`),
      el("button", { onclick: async () => { await api("/auth/logout", { method: "POST" }); location.assign("/"); } }, "Sign out"),
    );
  } else {
    nav.append(el("a", { class: "button primary", href: "/auth/login" }, "Sign in with Hack Club"));
  }
  topBar.replaceChildren(el("a", { class: "logo", href: "/" }, "Pu", el("span", {}, "l"), "se"), nav);
}

function landing() {
  view.replaceChildren(
    el("div", { class: "hero" },
      el("h1", {}, "See the time you put into building with AI."),
      el("p", {}, "Pulse counts the hours you spend in the loop with a coding agent: prompting, reading, reviewing and steering. It runs quietly in the background and keeps your recordings on your own computer."),
      el("a", { class: "button primary", href: "/auth/login" }, "Sign in with Hack Club"),
    ),
  );
}

function chart(days) {
  const max = Math.max(...days.map((day) => day.seconds), 1);
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 600 110");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Time per day over the last 30 days");
  days.forEach((day, index) => {
    const height = day.seconds > 0 ? Math.max(4, (day.seconds / max) * 100) : 3;
    const bar = document.createElementNS(SVG, "rect");
    bar.setAttribute("x", index * 20 + 2);
    bar.setAttribute("y", 106 - height);
    bar.setAttribute("width", 16);
    bar.setAttribute("height", height);
    bar.setAttribute("rx", 3);
    if (day.seconds === 0) bar.setAttribute("class", "empty");
    const title = document.createElementNS(SVG, "title");
    title.textContent = `${day.date}: ${duration(day.seconds)}`;
    bar.append(title);
    svg.append(bar);
  });
  return el("div", { class: "chart" }, svg, el("div", { class: "axis" }, el("span", {}, days[0].date), el("span", {}, "today (UTC)")));
}

function projectRow(project, total, isOwner, reload) {
  // the server only sends a link for an exact GitHub repository address
  const linkable = project.url && project.url.startsWith("https://github.com/");
  const label = project.name
    ? el("span", { class: "name" },
        linkable ? el("a", { href: project.url, target: "_blank", rel: "noopener noreferrer" }, project.name) : project.name)
    : el("span", { class: "name private" }, "Private project");
  const share = el("div", { class: "bar" });
  share.style.width = `${Math.max(2, (project.seconds / Math.max(total, 1)) * 100)}%`;
  const toggle = isOwner
    ? el("label", {}, el("input", {
        type: "checkbox",
        checked: project.public,
        onchange: async (event) => {
          await api(`/api/projects/${project.id}`, { method: "PATCH", body: { public: event.target.checked } });
          reload();
        },
      }), "show name")
    : null;
  const pushed = project.pushes > 0 && project.last_push_at
    ? el("div", { class: "note" }, `${project.pushes} ${project.pushes === 1 ? "push" : "pushes"} to GitHub · last ${ago(project.last_push_at)}`)
    : null;
  return el("div", { class: "project" }, el("div", { class: "line" }, label, toggle, el("span", { class: "time" }, duration(project.seconds))), pushed, share);
}

function renderProfile(data, reload) {
  const total = data.projects.reduce((sum, project) => sum + project.seconds, 0);
  const section = el("section", {},
    el("h1", {}, data.display_name),
    el("div", { class: "handle" }, `@${data.username}`),
    data.building_now
      ? el("div", { class: "live" }, data.building_project ? `Building now · ${data.building_project}` : "Building now")
      : null,
    el("div", { class: "stats" },
      el("div", { class: "stat" }, el("b", {}, duration(data.total_seconds)), el("span", {}, "all time")),
      el("div", { class: "stat" }, el("b", {}, duration(data.today_seconds)), el("span", {}, "today")),
      el("div", { class: "stat" }, el("b", {}, `${data.current_streak} d`), el("span", {}, "current streak")),
      el("div", { class: "stat" }, el("b", {}, `${data.longest_streak} d`), el("span", {}, "longest streak")),
    ),
    el("h2", {}, "Last 30 days"),
    chart(data.days),
    el("h2", {}, "Projects"),
    data.projects.length
      ? data.projects.map((project) => projectRow(project, total, data.is_owner, reload))
      : el("p", { class: "empty-note" }, "No time logged yet."),
  );
  const tools = [...data.harnesses, ...data.models];
  if (tools.length) {
    section.append(el("h2", {}, "Built with"), el("div", { class: "chips" }, tools.map((tool) => el("span", { class: "chip" }, tool))));
  }
  return section;
}

function githubSection(me) {
  const { user, github } = me;
  const body = [];
  if (user.github_login) {
    body.push(
      el("div", { class: "row" },
        el("span", { class: "name" }, `Connected as ${user.github_login}`),
        el("button", { onclick: async () => { await api("/api/github", { method: "DELETE" }); location.reload(); } }, "Disconnect")),
      el("p", { class: "note" }, "Pushes to your repositories show up next to their projects. Pulse keeps no GitHub token, and disconnecting deletes the pushes it recorded."),
    );
    if (github.install_url) {
      body.push(el("p", { class: "note" }, el("a", { href: github.install_url, target: "_blank", rel: "noopener noreferrer" }, "Choose which repositories Pulse hears about")));
    }
  } else if (github.can_connect) {
    body.push(el("div", { class: "row" }, el("span", { class: "name" }, "Not connected"), el("a", { class: "button", href: "/auth/github" }, "Connect GitHub")));
  } else {
    body.push(el("p", { class: "empty-note" }, "Connecting GitHub is not set up on this server."));
  }
  return el("div", {}, el("h2", {}, "GitHub"), body);
}

function ownerPanel(me) {
  const list = el("div");
  const message = el("p", { class: "note" });
  const nameInput = el("input", { type: "text", placeholder: "Name this computer", maxlength: 60, "aria-label": "Computer name" });

  async function loadTokens() {
    const { tokens } = await api("/api/tokens");
    list.replaceChildren(...tokens.map((token) =>
      el("div", { class: "row" },
        el("span", { class: "name" }, token.name),
        el("span", { class: "time" }, token.last_used_at ? `used ${new Date(token.last_used_at * 1000).toLocaleDateString()}` : "not used yet"),
        el("button", { onclick: async () => { await api(`/api/tokens/${token.id}`, { method: "DELETE" }); loadTokens(); } }, "Revoke"),
      )));
    if (!tokens.length) list.replaceChildren(el("p", { class: "empty-note" }, "No computers connected yet."));
  }

  async function addToken() {
    message.className = "note";
    try {
      const made = await api("/api/tokens", { method: "POST", body: { name: nameInput.value } });
      nameInput.value = "";
      message.replaceChildren("Copy this token now. It is shown once.", el("code", { class: "token" }, made.token));
      loadTokens();
    } catch (error) {
      message.className = "error";
      message.textContent = error.message;
    }
  }

  loadTokens();
  return el("section", {},
    el("h2", {}, "Privacy"),
    el("label", { class: "note" }, el("input", {
      type: "checkbox",
      checked: me.user.profile_public,
      onchange: async (event) => { await api("/api/me", { method: "PATCH", body: { profile_public: event.target.checked } }); },
    }), " Anyone can see this profile. Project names stay hidden until you show them."),
    githubSection(me),
    el("h2", {}, "Computers"),
    list,
    el("div", { class: "row" }, nameInput, el("button", { onclick: addToken }, "Connect a computer")),
    message,
    el("p", { class: "note" }, "Pulse on your computer will ask for this token to send your time here. Recordings never leave your computer unless you submit them."),
  );
}

async function showProfile(username, me) {
  const holder = el("div");
  view.replaceChildren(holder);
  let timer;
  async function load() {
    try {
      const data = await api(`/api/users/${encodeURIComponent(username)}`);
      holder.replaceChildren(renderProfile(data, load));
      return data;
    } catch (error) {
      holder.replaceChildren(el("h1", {}, "No profile here"), el("p", { class: "empty-note" }, "This profile does not exist or is private."));
      throw error;
    }
  }
  const data = await load().catch(() => null);
  if (!data) return;
  if (data.is_owner) view.append(ownerPanel(me));

  const later = () => { clearTimeout(timer); timer = setTimeout(() => load().catch(() => {}), 400); };
  const source = new EventSource(`/api/users/${encodeURIComponent(username)}/live`);
  source.addEventListener("credit", later);
  source.addEventListener("refresh", later);
  // the "building now" badge goes away by itself when nothing new arrives, so look again every minute: 60 * 1000
  setInterval(() => load().catch(() => {}), 60000);
}

async function start() {
  const me = await api("/api/me");
  renderTop(me);
  const match = location.pathname.match(/^\/u\/([^/]+)$/);
  if (match) return showProfile(decodeURIComponent(match[1]), me);
  if (me.user) return location.replace(`/u/${encodeURIComponent(me.user.username)}`);
  landing();
}

start();
