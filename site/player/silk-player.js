// Silk player: backend-agnostic timing engine for .silk transmissions.
// Parses the JSONL cast, then paces "o" events onto every attached backend.
// Backends implement: reset(header), write(dataString), and optionally
// applyStage(header) and marker(label).

export class SilkPlayer {
  constructor() {
    this.backends = [];
    this.header = null;
    this.events = [];
    this.timer = null;
    this.startEpoch = 0;
    this.cursor = 0;
    this.speed = 1.0;
    this.playing = false;
    this.onprogress = null;
    this.onloop = null;
  }

  attach(backend) {
    this.backends.push(backend);
    if (this.header) backend.reset(this.header);
  }

  async load(url) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`fetch ${url}: ${response.status}`);
    const text = await response.text();
    const lines = text.split("\n").filter((line) => line.trim().length > 0);
    this.header = JSON.parse(lines[0]);
    this.events = lines.slice(1).map((line) => JSON.parse(line));
    this.duration = this.events.length
      ? this.events[this.events.length - 1][0]
      : 0;
    return this.header;
  }

  play() {
    if (!this.header || this.playing) return;
    this.playing = true;
    this.cursor = 0;
    this.startEpoch = performance.now();
    for (const backend of this.backends) {
      backend.reset(this.header);
      backend.applyStage?.(this.header);
    }
    this.tick();
  }

  stop() {
    this.playing = false;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  tick() {
    if (!this.playing) return;
    const elapsed = ((performance.now() - this.startEpoch) / 1000) * this.speed;

    while (
      this.cursor < this.events.length &&
      this.events[this.cursor][0] <= elapsed
    ) {
      const [, code, data] = this.events[this.cursor];
      if (code === "o") {
        for (const backend of this.backends) backend.write(data);
      } else if (code === "m") {
        for (const backend of this.backends) backend.marker?.(data);
      }
      this.cursor += 1;
    }

    this.onprogress?.(Math.min(elapsed, this.duration), this.duration);

    if (this.cursor >= this.events.length) {
      const loops = this.header.x_ratty?.loop === true;
      this.playing = false;
      if (loops) {
        this.onloop?.();
        // A breath between loops so the restart reads as intentional.
        this.timer = setTimeout(() => this.play(), 900 / this.speed);
      }
      return;
    }

    const nextAt = this.events[this.cursor][0];
    const waitMs = Math.max(0, (nextAt - elapsed) * (1000 / this.speed));
    this.timer = setTimeout(() => this.tick(), Math.min(waitMs, 50));
  }
}
