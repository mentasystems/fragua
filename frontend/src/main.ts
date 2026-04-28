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
  | { kind: "FootprintAdded" }
  | { kind: "FootprintMoved" }
  | { kind: "FootprintRemoved" }
  | { kind: "OutlineChanged" }
  | { kind: "SymbolAdded" }
  | { kind: "NetChanged" }
  | { kind: "RoutingChanged" }
  | { kind: "PlacementProgress" }
  | { kind: "PaletteChanged" };

type View = "board" | "schematic";

const root = document.getElementById("app");
if (!root) throw new Error("missing #app root");

root.innerHTML = `
  <div class="topbar">
    <span class="label">project</span><span class="value" id="proj-name">—</span>
    <span class="tabs">
      <button class="tab" data-view="schematic" id="tab-sch">schematic <span id="proj-symbols">0</span>/<span id="proj-nets">0</span></button>
      <button class="tab" data-view="board" id="tab-board">board <span id="proj-footprints">0</span></button>
    </span>
    <span class="board-size">
      <span class="label">size</span>
      <input type="number" id="board-w" min="1" step="1" value="50" />
      <span class="label">×</span>
      <input type="number" id="board-h" min="1" step="1" value="40" />
      <span class="label">mm</span>
      <button class="action" id="board-set" title="Set the board outline">set</button>
    </span>
    <button class="action" id="auto-place-btn" title="Run simulated-annealing placement on every palette item">auto-place</button>
    <button class="action" id="route-btn" title="Auto-route every net">route</button>
    <button class="action" id="export-btn" title="Write Gerbers + drill + BOM + pick-and-place to ~/Downloads">export…</button>
    <button class="action danger" id="reset-btn" title="Wipe schematic, palette, and board">reset</button>
    <span class="spacer"></span>
    <span class="label">mcp</span><span class="value accent" id="proj-mcp">—</span>
  </div>
  <div class="palette-strip" id="palette-strip"></div>
  <div class="canvas-pane" id="canvas-pane"></div>
  <div class="activity-pane">
    <h2>activity</h2>
    <div class="activity-log" id="activity-log"></div>
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
  tabBoard: document.getElementById("tab-board")!,
  tabSch: document.getElementById("tab-sch")!,
  palette: document.getElementById("palette-strip")!,
  autoPlace: document.getElementById("auto-place-btn")! as HTMLButtonElement,
  route: document.getElementById("route-btn")! as HTMLButtonElement,
  export: document.getElementById("export-btn")! as HTMLButtonElement,
  reset: document.getElementById("reset-btn")! as HTMLButtonElement,
  boardW: document.getElementById("board-w")! as HTMLInputElement,
  boardH: document.getElementById("board-h")! as HTMLInputElement,
  boardSet: document.getElementById("board-set")! as HTMLButtonElement,
};

let view: View = "board";
let lastState: ProjectState | null = null;
let hoveredRef: string | null = null;
let selectedRef: string | null = null;

function setView(v: View) {
  view = v;
  els.tabBoard.classList.toggle("active", v === "board");
  els.tabSch.classList.toggle("active", v === "schematic");
  if (lastState) paintCanvas(lastState);
}

els.tabBoard.addEventListener("click", () => setView("board"));
els.tabSch.addEventListener("click", () => setView("schematic"));

els.boardSet.addEventListener("click", async () => {
  const w = Number(els.boardW.value);
  const h = Number(els.boardH.value);
  if (!Number.isFinite(w) || !Number.isFinite(h) || w < 1 || h < 1) {
    appendActivity("error", "size must be ≥1 mm");
    return;
  }
  try {
    setView("board");
    await invoke("set_board_outline", { wMm: w, hMm: h });
  } catch (err) {
    appendActivity("error", String(err));
  }
});

els.autoPlace.addEventListener("click", async () => {
  els.autoPlace.disabled = true;
  try {
    setView("board");
    await invoke("run_auto_placement");
  } catch (err) {
    appendActivity("error", String(err));
  } finally {
    els.autoPlace.disabled = false;
  }
});

els.route.addEventListener("click", async () => {
  els.route.disabled = true;
  try {
    setView("board");
    const summary = await invoke<string>("run_router");
    appendActivity("info", summary);
  } catch (err) {
    appendActivity("error", String(err));
  } finally {
    els.route.disabled = false;
  }
});

els.export.addEventListener("click", async () => {
  els.export.disabled = true;
  try {
    const dir = await invoke<string>("export_fab_pack");
    appendActivity("info", `exported to ${dir}`);
  } catch (err) {
    appendActivity("error", String(err));
  } finally {
    els.export.disabled = false;
  }
});

els.reset.addEventListener("click", async () => {
  // No confirm() here — Tauri's webview swallows JS dialogs and the
  // button used to silently no-op. The activity log line afterward
  // is the receipt.
  try {
    await invoke("reset_project");
    appendActivity("info", "project reset (UI button)");
  } catch (err) {
    appendActivity("error", String(err));
  }
});

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
  if (view === "board") attachBoardPointerHandlers();
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
    els.boardW.value = String(Math.round(state.outline.w_mm));
    els.boardH.value = String(Math.round(state.outline.h_mm));
  }
  paintPalette(state);
  paintCanvas(state);
}

async function start() {
  setView("board");
  appendActivity("info", "ui boot");
  await refresh();
  appendActivity("info", "ui ready");

  await listen<AnyEvent>("pcb://event", async (ev) => {
    try {
      const data = ev.payload;
      if (data.kind === "Activity") {
        appendActivity(data.level, data.message);
        return;
      }
      if (
        data.kind === "PlacementProgress" ||
        data.kind === "RoutingChanged" ||
        data.kind === "FootprintAdded" ||
        data.kind === "FootprintMoved" ||
        data.kind === "FootprintRemoved" ||
        data.kind === "OutlineChanged"
      ) {
        if (view !== "board") setView("board");
      }
      await refresh();
    } catch (err) {
      reportFatal(err);
    }
  });
}

start().catch(reportFatal);
