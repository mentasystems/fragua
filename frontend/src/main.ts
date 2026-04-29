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
  mcp_addr: string;
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
    <span class="label">mcp</span><span class="value accent" id="proj-mcp">—</span>
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
`;

const els = {
  name: document.getElementById("proj-name")!,
  symbols: document.getElementById("proj-symbols")!,
  nets: document.getElementById("proj-nets")!,
  footprints: document.getElementById("proj-footprints")!,
  mcp: document.getElementById("proj-mcp")!,
  canvas: document.getElementById("canvas-pane")!,
  log: document.getElementById("activity-log")!,
  library: document.getElementById("library-list")!,
  libraryCount: document.getElementById("library-count")!,
  tabBoard: document.getElementById("tab-board")!,
  tabSch: document.getElementById("tab-sch")!,
  palette: document.getElementById("palette-strip")!,
  boardW: document.getElementById("board-w")!,
  boardH: document.getElementById("board-h")!,
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
let hoveredRef: string | null = null;
let selectedRef: string | null = null;
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

function paintCanvas(state: ProjectState) {
  els.canvas.innerHTML = view === "schematic" ? state.schematic_svg : state.board_svg;
  if (view === "board") {
    attachBoardPointerHandlers();
    paintDrcMarkers();
  }
}

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
    attachChipDrag(chip, item.reference);
    els.palette.appendChild(chip);
  }
}

/// Pointer-event drag for palette chips. Robust across webviews; the
/// HTML5 DnD API has too many ways to silently no-op inside Tauri.
function attachChipDrag(chip: HTMLElement, reference: string) {
  chip.addEventListener("pointerdown", (ev) => {
    ev.preventDefault();
    setView("board");
    const ghost = document.createElement("div");
    ghost.className = "drag-ghost";
    ghost.textContent = reference;
    document.body.appendChild(ghost);
    const moveGhost = (e: PointerEvent) => {
      ghost.style.left = `${e.clientX + 12}px`;
      ghost.style.top = `${e.clientY + 12}px`;
    };
    moveGhost(ev);

    const onMove = (e: PointerEvent) => moveGhost(e);
    const onUp = async (e: PointerEvent) => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      ghost.remove();
      // Hit-test: was the drop on the canvas?
      const target = document.elementFromPoint(e.clientX, e.clientY);
      if (!target?.closest("#canvas-pane")) return;
      if (!lastState) return;
      const mm = clientToBoardMm(lastState, e.clientX, e.clientY);
      if (!mm) return;
      try {
        await invoke("place_from_palette", { reference, xMm: mm.x, yMm: mm.y });
      } catch (err) {
        appendActivity("error", `place_from_palette(${reference}): ${err}`);
      }
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  });
}

/// Translate a clientX/clientY (mouse coords on canvas) into board mm.
function clientToBoardMm(state: ProjectState, clientX: number, clientY: number): { x: number; y: number } | null {
  if (!state.outline) return null;
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (!svg) return null;
  const pt = svg.createSVGPoint();
  pt.x = clientX;
  pt.y = clientY;
  const ctm = svg.getScreenCTM();
  if (!ctm) return null;
  const local = pt.matrixTransform(ctm.inverse());
  // Inner <g> uses scale(1,-1); undo it so we get board mm.
  return { x: local.x, y: -local.y };
}

/// Drag-to-move for footprints already on the board, plus
/// drag-to-resize for the outline handles, plus hover tracking so the
/// "R" keyboard shortcut knows which footprint to rotate.
function attachBoardPointerHandlers() {
  const svg = els.canvas.querySelector("svg") as SVGSVGElement | null;
  if (!svg) return;

  // Track which footprint the cursor is over for keyboard rotation.
  svg.addEventListener("pointermove", (ev) => {
    const target = ev.target as Element;
    const ref = target.closest("[data-board-ref]")?.getAttribute("data-board-ref") ?? null;
    hoveredRef = ref;
  });
  svg.addEventListener("pointerleave", () => { hoveredRef = null; });

  svg.addEventListener("pointerdown", (ev) => {
    const target = ev.target as Element;

    // Click on a footprint? Select it (also drag-starts below).
    const refClicked = target.closest("[data-board-ref]")?.getAttribute("data-board-ref") ?? null;
    if (refClicked) {
      selectedRef = refClicked;
    } else {
      selectedRef = null;
    }

    // Outline resize handle?
    const edge = target.closest("[data-resize-edge]")?.getAttribute("data-resize-edge");
    if (edge && lastState?.outline) {
      ev.preventDefault();
      const startOutline = lastState.outline;
      const start = clientToBoardMm(lastState, ev.clientX, ev.clientY);
      if (!start) return;
      let lastSent = 0;
      const compute = (mx: number, my: number): { w: number; h: number } => {
        let w = startOutline.w_mm;
        let h = startOutline.h_mm;
        switch (edge) {
          case "right":  w = Math.max(1, mx - startOutline.x_mm); break;
          case "left":   w = Math.max(1, startOutline.x_mm + startOutline.w_mm - mx); break;
          case "top":    h = Math.max(1, my - startOutline.y_mm); break;
          case "bottom": h = Math.max(1, startOutline.y_mm + startOutline.h_mm - my); break;
        }
        return { w, h };
      };
      const onMove = async (e: PointerEvent) => {
        const now = performance.now();
        if (now - lastSent < 50) return;
        lastSent = now;
        if (!lastState) return;
        const mm = clientToBoardMm(lastState, e.clientX, e.clientY);
        if (!mm) return;
        const { w, h } = compute(mm.x, mm.y);
        try { await invoke("set_board_outline", { wMm: w, hMm: h }); } catch { /* */ }
      };
      const onUp = async (e: PointerEvent) => {
        document.removeEventListener("pointermove", onMove);
        document.removeEventListener("pointerup", onUp);
        if (!lastState) return;
        const mm = clientToBoardMm(lastState, e.clientX, e.clientY);
        if (!mm) return;
        const { w, h } = compute(mm.x, mm.y);
        try { await invoke("set_board_outline", { wMm: w, hMm: h }); } catch (err) {
          appendActivity("error", `resize: ${err}`);
        }
      };
      document.addEventListener("pointermove", onMove);
      document.addEventListener("pointerup", onUp);
      // Suppress the rest of the unused-var warning
      void start;
      return;
    }

    // Footprint drag?
    const ref = target.closest("[data-board-ref]")?.getAttribute("data-board-ref");
    if (!ref) return;
    ev.preventDefault();
    let lastSent = 0;
    const onMove = async (e: PointerEvent) => {
      const now = performance.now();
      if (now - lastSent < 33) return;
      lastSent = now;
      if (!lastState) return;
      const mm = clientToBoardMm(lastState, e.clientX, e.clientY);
      if (!mm) return;
      try { await invoke("move_footprint", { reference: ref, xMm: mm.x, yMm: mm.y }); } catch { /* */ }
    };
    const onUp = async (e: PointerEvent) => {
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
      if (!lastState) return;
      const mm = clientToBoardMm(lastState, e.clientX, e.clientY);
      if (!mm) return;
      try { await invoke("move_footprint", { reference: ref, xMm: mm.x, yMm: mm.y }); } catch (err) {
        appendActivity("error", `move(${ref}): ${err}`);
      }
    };
    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  });
}

document.addEventListener("keydown", async (ev) => {
  // Ignore key events from inputs (board size, etc).
  const target = ev.target as Element | null;
  if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA") return;
  if (ev.key.toLowerCase() === "r") {
    const ref = selectedRef ?? hoveredRef;
    if (!ref) return;
    ev.preventDefault();
    const delta = ev.shiftKey ? -90 : 90;
    try {
      await invoke("rotate_footprint", { reference: ref, degreesDelta: delta });
    } catch (err) {
      appendActivity("error", `rotate(${ref}): ${err}`);
    }
  }
});

async function refresh() {
  const state = await invoke<ProjectState>("project_state");
  lastState = state;
  els.name.textContent = state.name;
  els.symbols.textContent = String(state.symbol_count);
  els.nets.textContent = String(state.net_count);
  els.footprints.textContent = String(state.footprint_count);
  els.mcp.textContent = state.mcp_addr;
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
