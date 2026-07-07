// Null backend: a minimal text-mode terminal plus the raw byte stream.
//
// Two surfaces, same bytes:
//  - a cell grid that interprets just enough ANSI (CUP, SGR truecolor,
//    clear) to make transmissions watchable without a GPU
//  - a raw tap that shows the ratty language itself scrolling by
//
// APC sequences (RGP, Kitty) are recognized and skipped by the grid but kept
// visible in the raw tap — that contrast is the point.

const ESC = "\x1b";

export class NullBackend {
  constructor(gridElement, rawElement) {
    this.gridElement = gridElement;
    this.rawElement = rawElement;
    this.cols = 104;
    this.rows = 32;
    this.pending = "";
    this.rawBuffer = "";
    this.renderQueued = false;
    this.resetState();
  }

  resetState() {
    this.grid = Array.from({ length: this.rows }, () => this.blankRow());
    this.row = 0;
    this.col = 0;
    this.fg = null;
    this.bg = null;
    this.bold = false;
  }

  blankRow() {
    return Array.from({ length: this.cols }, () => ({ ch: " ", fg: null, bg: null, bold: false }));
  }

  reset(header) {
    this.cols = header.width || 104;
    this.rows = header.height || 32;
    this.pending = "";
    this.rawBuffer = "";
    this.resetState();
    this.scheduleRender();
  }

  write(data) {
    this.tapRaw(data);
    this.pending += data;
    this.consume();
    this.scheduleRender();
  }

  tapRaw(data) {
    // Make control bytes legible: the stream is a design element.
    const legible = data.replaceAll(ESC, "␛");
    this.rawBuffer = (this.rawBuffer + legible).slice(-4000);
    if (this.rawElement) {
      this.rawElement.textContent = this.rawBuffer;
      this.rawElement.scrollTop = this.rawElement.scrollHeight;
    }
  }

  consume() {
    let input = this.pending;
    let index = 0;
    while (index < input.length) {
      const ch = input[index];
      if (ch !== ESC) {
        this.putChar(ch);
        index += 1;
        continue;
      }
      // Escape sequence. If incomplete, stash the tail for the next write.
      const rest = input.slice(index);
      const consumed = this.consumeEscape(rest);
      if (consumed === 0) {
        this.pending = rest;
        return;
      }
      index += consumed;
    }
    this.pending = "";
  }

  // Returns consumed byte count, or 0 when the sequence is incomplete.
  consumeEscape(seq) {
    if (seq.length < 2) return 0;
    const kind = seq[1];
    if (kind === "[") {
      const match = seq.match(/^\x1b\[([0-9;:?<>=]*)([A-Za-z@`~])/);
      if (!match) return seq.length > 24 ? 2 : 0;
      this.applyCsi(match[1], match[2]);
      return match[0].length;
    }
    if (kind === "_" || kind === "P" || kind === "]") {
      // APC / DCS / OSC: skip to terminator (ESC \ or BEL for OSC).
      const st = seq.indexOf(`${ESC}\\`);
      const bel = kind === "]" ? seq.indexOf("\x07") : -1;
      if (st === -1 && bel === -1) return 0;
      if (st === -1) return bel + 1;
      if (bel === -1) return st + 2;
      return Math.min(st + 2, bel + 1);
    }
    return 2; // Two-byte escape we do not model.
  }

  applyCsi(params, final) {
    const parts = params.split(";").map((part) => parseInt(part, 10));
    if (final === "H" || final === "f") {
      this.row = Math.min(this.rows - 1, Math.max(0, (parts[0] || 1) - 1));
      this.col = Math.min(this.cols - 1, Math.max(0, (parts[1] || 1) - 1));
    } else if (final === "J") {
      this.grid = Array.from({ length: this.rows }, () => this.blankRow());
    } else if (final === "m") {
      this.applySgr(params.split(";"));
    }
  }

  applySgr(codes) {
    for (let index = 0; index < codes.length; index += 1) {
      const code = parseInt(codes[index], 10) || 0;
      if (code === 0) {
        this.fg = null;
        this.bg = null;
        this.bold = false;
      } else if (code === 1) {
        this.bold = true;
      } else if (code === 38 && codes[index + 1] === "2") {
        this.fg = `rgb(${codes[index + 2]},${codes[index + 3]},${codes[index + 4]})`;
        index += 4;
      } else if (code === 48 && codes[index + 1] === "2") {
        this.bg = `rgb(${codes[index + 2]},${codes[index + 3]},${codes[index + 4]})`;
        index += 4;
      }
    }
  }

  putChar(ch) {
    if (ch === "\r") {
      this.col = 0;
      return;
    }
    if (ch === "\n") {
      this.row = Math.min(this.rows - 1, this.row + 1);
      return;
    }
    if (this.row < this.rows && this.col < this.cols) {
      this.grid[this.row][this.col] = {
        ch,
        fg: this.fg,
        bg: this.bg,
        bold: this.bold,
      };
    }
    this.col += 1;
  }

  marker() {}

  scheduleRender() {
    if (this.renderQueued || !this.gridElement) return;
    this.renderQueued = true;
    requestAnimationFrame(() => {
      this.renderQueued = false;
      this.render();
    });
  }

  render() {
    const out = [];
    for (const row of this.grid) {
      let line = "";
      let open = null;
      for (const cell of row) {
        const style = this.cellStyle(cell);
        if (style !== open) {
          if (open !== null) line += "</span>";
          if (style !== null) line += `<span style="${style}">`;
          open = style;
        }
        line += escapeHtml(cell.ch);
      }
      if (open !== null) line += "</span>";
      out.push(line);
    }
    this.gridElement.innerHTML = out.join("\n");
  }

  cellStyle(cell) {
    if (!cell.fg && !cell.bg && !cell.bold) return null;
    let style = "";
    if (cell.fg) style += `color:${cell.fg};`;
    if (cell.bg) style += `background:${cell.bg};`;
    if (cell.bold) style += "font-weight:700;";
    return style;
  }
}

function escapeHtml(ch) {
  if (ch === "&") return "&amp;";
  if (ch === "<") return "&lt;";
  if (ch === ">") return "&gt;";
  return ch;
}
