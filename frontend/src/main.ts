import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type ProjectState = {
  name: string;
  footprint_count: number;
  mcp_addr: string;
  svg: string;
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
  | { kind: "OutlineChanged" };

const root = document.getElementById("app");
if (!root) {
  throw new Error("missing #app root");
}

root.innerHTML = `
  <div class="topbar">
    <span class="label">project</span><span class="value" id="proj-name">—</span>
    <span class="label">footprints</span><span class="value" id="proj-count">0</span>
    <span class="label">mcp</span><span class="value accent" id="proj-mcp">—</span>
  </div>
  <div class="canvas-pane" id="canvas-pane"></div>
  <div class="activity-pane">
    <h2>activity</h2>
    <div class="activity-log" id="activity-log"></div>
  </div>
`;

const els = {
  name: document.getElementById("proj-name")!,
  count: document.getElementById("proj-count")!,
  mcp: document.getElementById("proj-mcp")!,
  canvas: document.getElementById("canvas-pane")!,
  log: document.getElementById("activity-log")!,
};

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
  // Also surface the error in the canvas pane so the user sees it even
  // if the activity panel is hidden or empty.
  els.canvas.innerHTML = `<pre style="padding:12px;color:#f85149;white-space:pre-wrap;font-size:12px;">${msg}</pre>`;
  console.error(err);
}

async function refresh() {
  const state = await invoke<ProjectState>("project_state");
  els.name.textContent = state.name;
  els.count.textContent = String(state.footprint_count);
  els.mcp.textContent = state.mcp_addr;
  els.canvas.innerHTML = state.svg;
}

async function start() {
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
      await refresh();
    } catch (err) {
      reportFatal(err);
    }
  });
}

start().catch(reportFatal);
