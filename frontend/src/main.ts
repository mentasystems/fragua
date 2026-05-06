import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type PaletteItem = {
  reference: string;
  value: string;
  library: string;
  pad_count: number;
};

type Outline = { x_mm: number; y_mm: number; w_mm: number; h_mm: number };

type ProjectState = {
  name: string;
  footprint_count: number;
  symbol_count: number;
  net_count: number;
  palette_count: number;
  palette: PaletteItem[];
  api_addr: string;
  board_svg: string;
  schematic_svg: string;
  outline: Outline | null;
};

type ActivityEvent = {
  kind: "Activity";
  level: "info" | "warn" | "error";
  message: string;
};

type AnyEvent =
  | ActivityEvent
  | { kind: "ProjectChanged" }
  | { kind: "FootprintAdded"; reference: string }
  | { kind: "FootprintMoved" }
  | { kind: "FootprintRemoved" }
  | { kind: "OutlineChanged" }
  | { kind: "SymbolAdded"; reference: string }
  | { kind: "NetChanged" }
  | { kind: "RoutingChanged" }
  | { kind: "PlacementProgress" }
  | { kind: "PaletteChanged" }
  | { kind: "LibraryChanged"; count: number };

type LibraryAttachment = {
  id: string;
  kind: string;
  filename: string;
  mime: string;
  added_at: number;
};

type LibraryEntry = {
  key: string;
  description: string;
  default_value: string;
  default_rotation_deg: number;
  edge_mounted: boolean;
  pad_count: number;
  attachments: LibraryAttachment[];
  created_at: number;
};

type View = "board" | "schematic";

const root = document.getElementById("app");
if (!root) throw new Error("missing #app root");

root.innerHTML = `
  <div class="topbar">
    <span class="label">project</span><span class="value" id="proj-name">—</span>
    <span class="tabs">
      <span class="tab" data-view="schematic" id="tab-sch">schematic <span id="proj-symbols">0</span>/<span id="proj-nets">0</span></span>
      <span class="tab" data-view="board" id="tab-board">board <span id="proj-footprints">0</span></span>
    </span>
    <span class="board-size">
      <span class="label">size</span>
      <span class="value" id="board-w">—</span>
      <span class="label">×</span>
      <span class="value" id="board-h">—</span>
      <span class="label">mm</span>
    </span>
    <span class="spacer"></span>
    <span class="tab" id="toggle-library" title="show/hide library panel">lib</span>
    <span class="tab" id="toggle-activity" title="show/hide activity log">log</span>
    <span class="label">api</span><span class="value accent" id="proj-api">—</span>
  </div>
  <div class="palette-strip" id="palette-strip"></div>
  <div class="canvas-pane" id="canvas-pane"></div>
  <div class="activity-pane">
    <h2>activity</h2>
    <div class="activity-log" id="activity-log"></div>
  </div>
  <div class="library-pane" id="library-pane">
    <h2>library <span id="library-count" class="value">0</span></h2>
    <div class="library-list" id="library-list"></div>
  </div>
  <div class="info-modal" id="info-modal" hidden>
    <div class="info-modal-card" id="info-modal-card"></div>
  </div>
`;

const els = {
  name: document.getElementById("proj-name")!,
  symbols: document.getElementById("proj-symbols")!,
  nets: document.getElementById("proj-nets")!,
  footprints: document.getElementById("proj-footprints")!,
  api: document.getElementById("proj-api")!,
  canvas: document.getElementById("canvas-pane")!,
  log: document.getElementById("activity-log")!,
  library: document.getElementById("library-list")!,
  libraryCount: document.getElementById("library-count")!,
  tabBoard: document.getElementById("tab-board")!,
  tabSch: document.getElementById("tab-sch")!,
  toggleLibrary: document.getElementById("toggle-library")!,
  toggleActivity: document.getElementById("toggle-activity")!,
  palette: document.getElementById("palette-strip")!,
  boardW: document.getElementById("board-w")!,
  boardH: document.getElementById("board-h")!,
  infoModal: document.getElementById("info-modal")!,
  infoCard: document.getElementById("info-modal-card")!,
};

type DrcViolation = {
  kind: string;
  severity: string;
  message: string;
  x_mm: number;
  y_mm: number;
  involved: string[];
};

let view: View = "board";
let lastState: ProjectState | null = null;
let drcViolations: DrcViolation[] = [];

function setView(v: View) {
  view = v;
  els.tabBoard.classList.toggle("active", v === "board");
  els.tabSch.classList.toggle("active", v === "schematic");
  if (lastState) paintCanvas(lastState);
}

// All control surface lives behind the agent now. Tabs stay clickable
// so the human can flip between the board and the schematic to watch,
// but every action (place, move, route, DRC, export, reset) goes
// through MCP — no UI buttons.
els.tabBoard.addEventListener("click", () => setView("board"));
els.tabSch.addEventListener("click", () => setView("schematic"));

// Side-pane toggles. Default: both panels hidden — the agent log /
// library panes get in the way for most tasks; the human can flip them
// open with these tabs. Choice is remembered in localStorage.
function applyPaneToggle(key: "library" | "activity", visible: boolean) {
  const cls = key === "library" ? "hide-library" : "hide-activity";
  const tab = key === "library" ? els.toggleLibrary : els.toggleActivity;
  root!.classList.toggle(cls, !visible);
  tab.classList.toggle("active", visible);
  localStorage.setItem(`fragua.pane.${key}`, visible ? "1" : "0");
}
function readPanePref(key: "library" | "activity"): boolean {
  const stored = localStorage.getItem(`fragua.pane.${key}`);
  return stored === "1"; // default = hidden
}
applyPaneToggle("library", readPanePref("library"));
applyPaneToggle("activity", readPanePref("activity"));
els.toggleLibrary.addEventListener("click", () =>
  applyPaneToggle("library", root!.classList.contains("hide-library")),
);
els.toggleActivity.addEventListener("click", () =>
  applyPaneToggle("activity", root!.classList.contains("hide-activity")),
);

function appendActivity(level: string, message: string) {
  const entry = document.createElement("div");
  entry.className = `entry ${level}`;
  entry.innerHTML = `<span class="level"></span><span class="msg"></span>`;
  entry.querySelector(".level")!.textContent = level;
  entry.querySelector(".msg")!.textContent = message;
  els.log.appendChild(entry);
  els.log.scrollTop = els.log.scrollHeight;
}

function reportFatal(err: unknown) {
  const msg = err instanceof Error ? `${err.message}\n${err.stack ?? ""}` : String(err);
  appendActivity("error", msg);
  els.canvas.innerHTML = `<pre style="padding:12px;color:#f85149;white-space:pre-wrap;font-size:12px;">${msg}</pre>`;
  console.error(err);
}

/// Per-view viewBox state so a re-render (every project change) does
/// not blow away the user's pan/zoom. Lazily seeded from whatever the
/// renderer emits on first paint of each view.
type ViewBox = { x: number; y: number; w: number; h: number };
const viewBoxState: Record<View, ViewBox | null> = { board: null, schematic: null };

function paintCanvas(state: ProjectState) {
  els.canvas.innerHTML = view === "schematic" ? state.schematic_svg : state.board_svg;
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (svg) {
    // Capture or restore the per-view viewBox so the user's pan/zoom
    // survives the paint, even when the SVG itself got rebuilt server-side.
    const fresh = parseViewBox(svg.getAttribute("viewBox"));
    if (viewBoxState[view] && fresh) {
      applyViewBox(svg, viewBoxState[view]!);
    } else if (fresh) {
      viewBoxState[view] = fresh;
    }
    attachPanZoom(svg);
  }
  if (view === "board") {
    paintDrcMarkers();
  }
}

function parseViewBox(s: string | null): ViewBox | null {
  if (!s) return null;
  const parts = s.split(/\s+/).map(parseFloat);
  if (parts.length !== 4 || parts.some(Number.isNaN)) return null;
  return { x: parts[0], y: parts[1], w: parts[2], h: parts[3] };
}

function applyViewBox(svg: SVGSVGElement, vb: ViewBox) {
  svg.setAttribute("viewBox", `${vb.x} ${vb.y} ${vb.w} ${vb.h}`);
}

/// Click-drag = pan, wheel = zoom around cursor, plain click on a
/// component → info modal. Operates directly on the SVG `viewBox` so
/// the pan/zoom is purely cosmetic — no reflow, no server roundtrip.
function attachPanZoom(svg: SVGSVGElement) {
  svg.style.cursor = "grab";

  svg.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    svg.setPointerCapture(ev.pointerId);
    svg.style.cursor = "grabbing";
    const start = viewBoxState[view] ?? parseViewBox(svg.getAttribute("viewBox"));
    if (!start) return;
    const px0 = ev.clientX;
    const py0 = ev.clientY;
    const rect = svg.getBoundingClientRect();
    const sx = start.w / rect.width;
    const sy = start.h / rect.height;
    // Walk up to the nearest <g data-board-ref> so a slow click on a
    // pad still resolves the parent component.
    const downTarget = ev.target as Element;
    const refOnDown = downTarget.closest?.("[data-board-ref]") as Element | null;
    let panned = false;
    const onMove = (e: PointerEvent) => {
      const dx = (e.clientX - px0) * sx;
      const dy = (e.clientY - py0) * sy;
      // 4 px threshold — small wobble during a click should not pan.
      if (!panned && Math.hypot(e.clientX - px0, e.clientY - py0) < 4) return;
      panned = true;
      const next: ViewBox = { x: start.x - dx, y: start.y - dy, w: start.w, h: start.h };
      viewBoxState[view] = next;
      applyViewBox(svg, next);
    };
    const onUp = () => {
      svg.removeEventListener("pointermove", onMove);
      svg.removeEventListener("pointerup", onUp);
      svg.removeEventListener("pointercancel", onUp);
      svg.style.cursor = "grab";
      if (!panned && refOnDown) {
        const ref = refOnDown.getAttribute("data-board-ref") ?? "";
        const key = refOnDown.getAttribute("data-library-key") ?? "";
        if (ref) void openComponentModal(ref, key);
      }
    };
    svg.addEventListener("pointermove", onMove);
    svg.addEventListener("pointerup", onUp);
    svg.addEventListener("pointercancel", onUp);
  });

  svg.addEventListener(
    "wheel",
    (ev) => {
      ev.preventDefault();
      const current = viewBoxState[view] ?? parseViewBox(svg.getAttribute("viewBox"));
      if (!current) return;
      const rect = svg.getBoundingClientRect();
      // Cursor in SVG units (anchor of the zoom).
      const fx = current.x + ((ev.clientX - rect.left) / rect.width) * current.w;
      const fy = current.y + ((ev.clientY - rect.top) / rect.height) * current.h;
      // Wheel up → zoom IN (smaller viewBox); wheel down → zoom OUT.
      const k = Math.exp(ev.deltaY * 0.0015);
      const minSpan = 1; // mm — don't zoom past ~1 mm per pane.
      const maxSpan = 5000;
      const newW = clamp(current.w * k, minSpan, maxSpan);
      const newH = clamp(current.h * k, minSpan, maxSpan);
      const next: ViewBox = {
        x: fx - ((fx - current.x) * newW) / current.w,
        y: fy - ((fy - current.y) * newH) / current.h,
        w: newW,
        h: newH,
      };
      viewBoxState[view] = next;
      applyViewBox(svg, next);
    },
    { passive: false },
  );
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

type ComponentInfo = {
  reference: string;
  key: string;
  value: string;
  description: string;
  rotation_deg: number;
  edge_mounted: boolean;
  x_mm: number;
  y_mm: number;
  pads: { number: string; name: string; net: string | null; layer: string }[];
  library: {
    key: string;
    description: string;
    default_value: string;
    edge_mounted: boolean;
    pad_count: number;
    attachments: { id: string; kind: string; filename: string; mime: string }[];
  } | null;
};

async function openComponentModal(reference: string, key: string) {
  els.infoCard.innerHTML = `<div class="info-loading">loading ${reference}…</div>`;
  els.infoModal.removeAttribute("hidden");
  let info: ComponentInfo;
  try {
    info = await invoke<ComponentInfo>("component_info", { reference });
  } catch (err) {
    els.infoCard.innerHTML = `<div class="info-error">component_info(${reference}): ${err}</div>`;
    return;
  }
  void key;

  const lib = info.library;
  const photos = lib?.attachments?.filter((a) => /^image\//.test(a.mime)) ?? [];
  const padsByLayer = info.pads.reduce<Record<string, number>>((m, p) => {
    m[p.layer] = (m[p.layer] ?? 0) + 1;
    return m;
  }, {});

  const head = `
    <header>
      <div>
        <div class="info-key">${esc(lib?.key || info.key || info.reference)}</div>
        <div class="info-ref">${esc(info.reference)}${info.value ? ` · ${esc(info.value)}` : ""}</div>
      </div>
      <button class="info-close" aria-label="close">×</button>
    </header>
  `;

  const dataCol = `
    <div class="info-data">
      <section class="info-meta">
        <div><span class="lbl">position</span><span class="val">${info.x_mm.toFixed(2)}, ${info.y_mm.toFixed(2)} mm</span></div>
        <div><span class="lbl">rotation</span><span class="val">${info.rotation_deg.toFixed(0)}°</span></div>
        <div><span class="lbl">edge-mount</span><span class="val">${info.edge_mounted ? "yes" : "no"}</span></div>
        <div><span class="lbl">pads</span><span class="val">${info.pads.length}${
          Object.keys(padsByLayer).length > 1
            ? ` (${Object.entries(padsByLayer).map(([l, n]) => `${n} ${l}`).join(", ")})`
            : ""
        }</span></div>
      </section>
      ${
        info.description || lib?.description
          ? `<section class="info-desc">${esc(info.description || lib?.description || "")}</section>`
          : ""
      }
      <section class="info-pads">
        <h3>pads</h3>
        <table>
          <thead><tr><th>#</th><th>name</th><th>net</th><th>layer</th></tr></thead>
          <tbody>${info.pads
            .map(
              (p) => `<tr>
              <td>${esc(p.number)}</td>
              <td>${esc(p.name || "—")}</td>
              <td>${esc(p.net || "—")}</td>
              <td>${esc(p.layer)}</td>
            </tr>`,
            )
            .join("")}</tbody>
        </table>
      </section>
    </div>
  `;

  const photoCol = photos.length > 0
    ? `<div class="info-photos" id="info-photos"></div>`
    : `<div class="info-photos empty">no photos</div>`;

  els.infoCard.innerHTML = head + `<div class="info-body">${dataCol}${photoCol}</div>`;
  els.infoCard.querySelector(".info-close")?.addEventListener("click", closeComponentModal);

  // Lazily fetch the photo attachments — these can be a few hundred
  // KB each so we kick them off after first paint.
  if (photos.length > 0 && lib) {
    const host = els.infoCard.querySelector("#info-photos") as HTMLElement | null;
    if (host) {
      for (const a of photos) {
        try {
          const uri = await invoke<string>("library_attachment_data_uri", {
            key: lib.key,
            attachmentId: a.id,
          });
          const img = document.createElement("img");
          img.src = uri;
          img.alt = a.filename;
          img.title = a.filename;
          host.appendChild(img);
        } catch (err) {
          appendActivity("error", `attachment ${a.filename}: ${err}`);
        }
      }
    }
  }
}

function closeComponentModal() {
  els.infoModal.setAttribute("hidden", "");
  els.infoCard.innerHTML = "";
}

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

document.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && !els.infoModal.hasAttribute("hidden")) {
    closeComponentModal();
  }
});

els.infoModal.addEventListener("click", (ev) => {
  if (ev.target === els.infoModal) closeComponentModal();
});

function paintDrcMarkers() {
  if (drcViolations.length === 0) return;
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (!svg) return;
  const inner = svg.querySelector("g[transform='scale(1,-1)']") as SVGGElement | null;
  const host = inner ?? svg;
  // SVG namespace required for createElementNS.
  const NS = "http://www.w3.org/2000/svg";
  const layer = document.createElementNS(NS, "g");
  layer.setAttribute("class", "drc-markers");
  for (const v of drcViolations) {
    const color = v.severity === "error" ? "#f85149" : "#d29922";
    const r = 1.2;
    const circle = document.createElementNS(NS, "circle");
    circle.setAttribute("cx", String(v.x_mm));
    circle.setAttribute("cy", String(v.y_mm));
    circle.setAttribute("r", String(r));
    circle.setAttribute("fill", "none");
    circle.setAttribute("stroke", color);
    circle.setAttribute("stroke-width", "0.25");
    const title = document.createElementNS(NS, "title");
    title.textContent = v.message;
    circle.appendChild(title);
    layer.appendChild(circle);
    // X mark inside.
    const len = r * 0.6;
    for (const [x1, y1, x2, y2] of [
      [v.x_mm - len, v.y_mm - len, v.x_mm + len, v.y_mm + len],
      [v.x_mm - len, v.y_mm + len, v.x_mm + len, v.y_mm - len],
    ] as const) {
      const line = document.createElementNS(NS, "line");
      line.setAttribute("x1", String(x1));
      line.setAttribute("y1", String(y1));
      line.setAttribute("x2", String(x2));
      line.setAttribute("y2", String(y2));
      line.setAttribute("stroke", color);
      line.setAttribute("stroke-width", "0.18");
      layer.appendChild(line);
    }
  }
  host.appendChild(layer);
}

function paintPalette(state: ProjectState) {
  els.palette.innerHTML = "";
  if (state.palette.length === 0) {
    els.palette.classList.add("empty");
    els.palette.textContent = "palette empty";
    return;
  }
  els.palette.classList.remove("empty");
  for (const item of state.palette) {
    const chip = document.createElement("div");
    chip.className = "palette-chip";
    chip.dataset.reference = item.reference;
    chip.innerHTML = `
      <span class="chip-ref"></span>
      <span class="chip-val"></span>
      <span class="chip-meta">${item.pad_count}p</span>
    `;
    chip.querySelector(".chip-ref")!.textContent = item.reference;
    chip.querySelector(".chip-val")!.textContent = item.value || item.library;
    // Read-only render: the palette is informational. Placement is
    // agent-driven via MCP (`place REF X Y` in the script).
    els.palette.appendChild(chip);
  }
}

async function refresh() {
  const state = await invoke<ProjectState>("project_state");
  lastState = state;
  els.name.textContent = state.name;
  els.symbols.textContent = String(state.symbol_count);
  els.nets.textContent = String(state.net_count);
  els.footprints.textContent = String(state.footprint_count);
  els.api.textContent = state.api_addr;
  if (state.outline) {
    els.boardW.textContent = String(Math.round(state.outline.w_mm));
    els.boardH.textContent = String(Math.round(state.outline.h_mm));
  } else {
    els.boardW.textContent = "—";
    els.boardH.textContent = "—";
  }
  paintPalette(state);
  paintCanvas(state);
}

let libraryThumbCache = new Map<string, string>(); // attachment_id → data URI

async function refreshLibrary() {
  const data = await invoke<{ entries: LibraryEntry[] }>("library_state");
  els.libraryCount.textContent = String(data.entries.length);
  els.library.innerHTML = "";
  if (data.entries.length === 0) {
    const empty = document.createElement("div");
    empty.className = "library-empty";
    empty.textContent = "no components yet — your agent will save parts here as you design";
    els.library.appendChild(empty);
    return;
  }
  for (const entry of data.entries) {
    const card = document.createElement("div");
    card.className = "library-card";
    card.dataset.key = entry.key;

    // Thumbnail = first photo attachment, if any.
    const thumb = document.createElement("div");
    thumb.className = "library-thumb";
    const photo = entry.attachments.find((a) =>
      a.mime.startsWith("image/")
    );
    if (photo) {
      const cached = libraryThumbCache.get(photo.id);
      if (cached) {
        thumb.style.backgroundImage = `url(${cached})`;
      } else {
        invoke<string>("library_attachment_data_uri", {
          key: entry.key,
          attachmentId: photo.id,
        })
          .then((uri) => {
            libraryThumbCache.set(photo.id, uri);
            thumb.style.backgroundImage = `url(${uri})`;
          })
          .catch(() => {});
      }
    } else {
      thumb.classList.add("library-thumb-empty");
      thumb.textContent = entry.key.slice(0, 2).toUpperCase();
    }
    card.appendChild(thumb);

    const body = document.createElement("div");
    body.className = "library-body";
    const title = document.createElement("div");
    title.className = "library-key";
    title.textContent = entry.key;
    body.appendChild(title);
    if (entry.default_value) {
      const val = document.createElement("div");
      val.className = "library-value";
      val.textContent = entry.default_value;
      body.appendChild(val);
    }
    const meta = document.createElement("div");
    meta.className = "library-meta";
    const parts = [`${entry.pad_count} pads`];
    if (entry.edge_mounted) parts.push("edge");
    if (entry.attachments.length > 0)
      parts.push(`${entry.attachments.length} attached`);
    meta.textContent = parts.join(" · ");
    body.appendChild(meta);
    if (entry.description) {
      const desc = document.createElement("div");
      desc.className = "library-desc";
      desc.textContent = entry.description;
      body.appendChild(desc);
    }
    card.appendChild(body);
    els.library.appendChild(card);
  }
}

// Animation pacing now lives on the BACKEND — it advances the visible
// state mirror one mutation per `ANIMATION_TICK_MS` and emits the
// matching event each time. The frontend just paints whatever arrives;
// no queueing here. Activity / Library / Project events come straight
// through the bus (not through the mirror) since they don't change the
// canvas, so they show up instantly which is fine.
async function playEvent(data: AnyEvent) {
  if (data.kind === "Activity") {
    appendActivity(data.level, data.message);
    return;
  }
  if (data.kind === "LibraryChanged") {
    // Library updates are independent of the board canvas; refresh
    // only the side panel so the view doesn't jump to "board".
    await refreshLibrary();
    return;
  }
  const isBoardEvent =
    data.kind === "PlacementProgress" ||
    data.kind === "RoutingChanged" ||
    data.kind === "FootprintAdded" ||
    data.kind === "FootprintMoved" ||
    data.kind === "FootprintRemoved" ||
    data.kind === "OutlineChanged";
  const isSchematicEvent =
    data.kind === "SymbolAdded" || data.kind === "NetChanged";
  if (isBoardEvent && view !== "board") setView("board");
  else if (isSchematicEvent && view !== "schematic") setView("schematic");
  await refresh();
  // Spawn flash on the footprint that just appeared. The DOM is fresh
  // after refresh() — find the matching <g data-board-ref> and tag it.
  if (data.kind === "FootprintAdded" && data.reference) {
    flashSpawn(`[data-board-ref="${cssEscape(data.reference)}"]`);
  }
  // Animate brand-new traces and vias: the render emits stable
  // `data-trace-id` / `data-via-id` attributes; anything we haven't
  // seen yet gets the spawn class so the trace draws in like a brush
  // stroke and the via fades/scales in.
  if (data.kind === "RoutingChanged") animateNewCopper();
  if (data.kind === "ProjectChanged") {
    seenTraceIds.clear();
    seenViaIds.clear();
  }
}

const seenTraceIds = new Set<string>();
const seenViaIds = new Set<string>();

function animateNewCopper() {
  document.querySelectorAll<SVGLineElement>("line[data-trace-id]").forEach((el) => {
    const id = el.getAttribute("data-trace-id");
    if (!id || seenTraceIds.has(id)) return;
    seenTraceIds.add(id);
    el.classList.add("trace-spawn");
  });
  document.querySelectorAll<SVGGElement>("g[data-via-id]").forEach((el) => {
    const id = el.getAttribute("data-via-id");
    if (!id || seenViaIds.has(id)) return;
    seenViaIds.add(id);
    el.classList.add("via-spawn");
  });
}

function flashSpawn(selector: string) {
  const node = document.querySelector(selector);
  if (!node) return;
  node.classList.remove("spawn"); // restart if already running
  // Force reflow so the next add triggers the keyframes again.
  void (node as HTMLElement).getBoundingClientRect();
  node.classList.add("spawn");
}

function cssEscape(s: string): string {
  // Just enough escaping for the references we generate (alphanum + a
  // few symbols); good enough so we don't pull in CSS.escape polyfills.
  return s.replace(/(["\\])/g, "\\$1");
}

async function start() {
  setView("board");
  appendActivity("info", "ui boot");
  await refresh();
  await refreshLibrary();
  appendActivity("info", "ui ready");

  await listen<AnyEvent>("pcb://event", (ev) => {
    playEvent(ev.payload).catch(reportFatal);
  });
}

start().catch(reportFatal);
