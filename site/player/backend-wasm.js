// Wasm backend: real ratty, compiled to WebAssembly, rendering on WebGPU.
// Loads lazily; while the module streams in, the null backend is already
// playing the same bytes — the canvas fades in over it when ready.

export class WasmBackend {
  constructor(canvasSelector, onready, onerror) {
    this.canvasSelector = canvasSelector;
    this.session = null;
    this.encoder = new TextEncoder();
    this.backlog = [];
    this.onready = onready;
    this.onerror = onerror;
    this.pollHandle = null;
  }

  static supported() {
    return typeof navigator !== "undefined" && !!navigator.gpu;
  }

  async boot(header) {
    if (!WasmBackend.supported()) {
      throw new Error("WebGPU unavailable");
    }
    const { default: init, start } = await import("../pkg/ratty.js");
    await init();
    this.session = start(this.canvasSelector, buildConfigToml(header));
    this.applyStage(header);
    // Replay whatever the null backend already played this loop.
    for (const data of this.backlog) this.feed(data);
    this.backlog = [];
    this.startInputPolling();
    this.onready?.();
  }

  startInputPolling() {
    const poll = () => {
      if (!this.session) return;
      // Drain terminal replies (RGP support responses, cursor reports);
      // surfaced on the console for now — the page has no keyboard loop yet.
      const bytes = this.session.drain_input();
      if (bytes.length > 0) {
        console.debug("ratty replied:", new TextDecoder().decode(bytes));
      }
      this.pollHandle = requestAnimationFrame(poll);
    };
    this.pollHandle = requestAnimationFrame(poll);
  }

  reset(header) {
    // A fresh loop: clear all inline objects and the screen.
    if (this.session) {
      this.feed("\x1b_ratty;g;d\x1b\\\x1b[2J\x1b[H");
      this.applyStage(header);
    }
  }

  applyStage(header) {
    const stage = header?.x_ratty;
    if (!this.session || !stage) return;
    if (stage.mode) this.session.set_mode(stage.mode);
    if (typeof stage.warp === "number") this.session.set_warp(stage.warp);
    if (stage.view) {
      this.session.set_view(
        stage.view.yaw ?? 0.18,
        stage.view.pitch ?? 0.08,
        stage.view.zoom ?? 1.0,
      );
    }
  }

  write(data) {
    if (!this.session) {
      this.backlog.push(data);
      return;
    }
    this.feed(data);
  }

  feed(data) {
    this.session.feed(this.encoder.encode(data));
  }

  marker() {}
}

// The cast header carries the stage; ratty's config carries the theme and
// grid. Bridge one into the other.
function buildConfigToml(header) {
  const lines = ["[terminal]"];
  lines.push(`default_cols = ${header.width || 104}`);
  lines.push(`default_rows = ${header.height || 32}`);
  const theme = header.theme;
  if (theme?.fg || theme?.bg) {
    lines.push("[theme]");
    if (theme.fg) lines.push(`foreground = "${theme.fg}"`);
    if (theme.bg) lines.push(`background = "${theme.bg}"`);
  }
  lines.push("[cursor.model]");
  lines.push("visible = true");
  return lines.join("\n");
}
