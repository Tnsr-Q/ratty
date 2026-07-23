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
    // Before the module boots: wrap the AudioContext constructors so the
    // context the wasm audio backend creates during startup (suspended by
    // browser autoplay policy) can be resumed on the first real gesture.
    const audioContexts = captureAudioContexts();
    const { default: init, start } = await import("../pkg/ratty.js");
    await init();
    this.session = start(this.canvasSelector, buildConfigToml(header));
    this.applyStage(header);
    // Replay whatever the null backend already played this loop.
    for (const data of this.backlog) this.feed(data);
    this.backlog = [];
    this.startInputPolling();
    this.installUnlockListeners(audioContexts);
    this.onready?.();
  }

  // Browser-autoplay unlock. Only an activation-granting gesture can
  // resume a suspended AudioContext, and only once one actually reaches
  // "running" do we report the gesture to the session (which unlocks the
  // sound organ and fades in a deferred ambient bed) and drop the
  // listeners. A single non-activating event (Escape, a lone modifier, a
  // pointerdown a browser does not count as activation) or a rejected
  // resume() must not consume the unlock and leave the organ reporting
  // unlocked while the context stays silent — so the listeners stay
  // installed, retrying on each gesture, until audio is genuinely live.
  // Pre-unlock is the normal first-load path: the first transmission
  // autoplays with no gesture.
  installUnlockListeners(audioContexts) {
    const events = ["pointerup", "click", "keydown", "touchend"];
    const isActivating = (event) => {
      if (event.type !== "keydown") return true;
      // Escape and lone modifier presses do not grant user activation.
      if (event.key === "Escape" || event.key === "Esc") return false;
      return !["Shift", "Control", "Alt", "Meta"].includes(event.key);
    };
    const finish = () => {
      for (const type of events) window.removeEventListener(type, tryUnlock);
      this.session?.user_gesture();
    };
    const tryUnlock = async (event) => {
      if (!isActivating(event)) return;
      const pending = audioContexts.filter((c) => c.state !== "running");
      // No context to resume (audio feature off, or already running):
      // the gesture stands on its own.
      if (pending.length === 0) {
        finish();
        return;
      }
      // resume() is invoked synchronously inside the handler so the user
      // activation still counts; we only await to learn whether it took.
      await Promise.all(pending.map((c) => c.resume().catch(() => {})));
      if (audioContexts.every((c) => c.state === "running")) finish();
    };
    for (const type of events) {
      window.addEventListener(type, tryUnlock, { passive: true });
    }
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

  // Read live terminal state over the OSC 778 query channel. Thin glue
  // only: the envelope, correlation, and decoding all live in Rust.
  // Resolves with the decoded JSON payload; rejects with an Error whose
  // `code` property is the stable wire code. Try from the console (the
  // page exposes the live backend as `window.ratty`):
  //   await ratty.query("caps")
  //   await ratty.query("state.visible_objects")
  query(op, data = null, timeoutMs = 2000) {
    if (!this.session) return Promise.reject(new Error("wasm session not booted"));
    return this.session.query(op, data, timeoutMs);
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

// Wraps window.AudioContext (and the webkit alias) so every context
// constructed after this call is recorded for gesture-time resume. The
// wasm side never sees these: the page owns the browser-policy dance, the
// terminal owns the unlock state. Idempotent — one shared capture list.
function captureAudioContexts() {
  if (window.__rattyAudioContexts) return window.__rattyAudioContexts;
  const captured = [];
  for (const name of ["AudioContext", "webkitAudioContext"]) {
    const Original = window[name];
    if (typeof Original !== "function") continue;
    const Captured = function (...args) {
      const context = new Original(...args);
      captured.push(context);
      return context;
    };
    Captured.prototype = Original.prototype;
    Object.setPrototypeOf(Captured, Original);
    window[name] = Captured;
  }
  window.__rattyAudioContexts = captured;
  return captured;
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
