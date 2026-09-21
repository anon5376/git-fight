const BG = "#0A0A0B";
const OURS = "#EEEEEA";
const THEIRS = "#FF4A1C";

/** 5×7 bitmaps, LSB = leftmost column. */
const GLYPHS: Record<string, readonly number[]> = {
  G: [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
  I: [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
  T: [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
  F: [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
  H: [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
  K: [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
  O: [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
  " ": [0, 0, 0, 0, 0, 0, 0],
};

export function drawDotTitle(canvas: HTMLCanvasElement, text: string): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }
  ctx.fillStyle = BG;
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  const dot = 6;
  const gap = 2;
  const cell = dot + gap;
  const glyphW = 5 * cell;
  const space = cell;
  let x = 8;
  const y = 12;
  ctx.fillStyle = OURS;
  for (const ch of text) {
    const rows = GLYPHS[ch] ?? GLYPHS[" "];
    for (let r = 0; r < 7; r += 1) {
      for (let c = 0; c < 5; c += 1) {
        if (rows[r] & (1 << (4 - c))) {
          ctx.beginPath();
          ctx.arc(x + c * cell + dot / 2, y + r * cell + dot / 2, dot / 2, 0, Math.PI * 2);
          ctx.fill();
        }
      }
    }
    x += glyphW + space;
  }
}

export type Frame = {
  oursName: string;
  theirsName: string;
  oursHp: number;
  theirsHp: number;
  oursMax: number;
  theirsMax: number;
  oursX: number;
  theirsX: number;
  oursSprite: string[];
  theirsSprite: string[];
  timer: string;
  roundLabel: string;
};

export function drawFrame(canvas: HTMLCanvasElement, frame: Frame): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.fillStyle = BG;
  ctx.fillRect(0, 0, w, h);
  ctx.font = '18px ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace';
  ctx.textBaseline = "top";

  ctx.fillStyle = OURS;
  ctx.fillText("GIT FIGHT", 24, 16);
  ctx.fillText(frame.roundLabel, 360, 16);
  ctx.fillText(frame.timer, w - 64, 16);

  ctx.fillText(padName(frame.oursName, 12), 24, 48);
  ctx.fillText(padName(frame.theirsName, 12).trimEnd(), w - 24 - 12 * 11, 48);
  drawBar(ctx, 24, 72, 280, frame.oursHp, frame.oursMax, OURS);
  drawBar(ctx, w - 304, 72, 280, frame.theirsHp, frame.theirsMax, THEIRS);
  ctx.fillStyle = OURS;
  ctx.fillText(String(frame.oursHp).padStart(3, " "), 310, 70);
  ctx.fillStyle = THEIRS;
  ctx.fillText(String(frame.theirsHp).padStart(3, " "), w - 348, 70);

  const cell = 14;
  const originY = 140;
  drawSprite(ctx, frame.oursSprite, 24 + frame.oursX * 8, originY, OURS, cell);
  drawSprite(ctx, frame.theirsSprite, 24 + frame.theirsX * 8, originY, THEIRS, cell);

  ctx.fillStyle = "#333";
  ctx.fillRect(24, originY + cell * 5 + 8, w - 48, 2);
  ctx.fillStyle = OURS;
  ctx.font = '14px ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace';
  ctx.fillText("a punch  s kick  d block  f special     j k l ;     q menu", 24, originY + cell * 5 + 24);
}

function drawSprite(
  ctx: CanvasRenderingContext2D,
  rows: string[],
  x: number,
  y: number,
  color: string,
  cell: number,
): void {
  ctx.fillStyle = color;
  ctx.font = `${cell}px ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace`;
  for (let i = 0; i < rows.length; i += 1) {
    ctx.fillText(rows[i] ?? "", x, y + i * cell);
  }
}

function drawBar(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  width: number,
  hp: number,
  max: number,
  color: string,
): void {
  ctx.fillStyle = "#222";
  ctx.fillRect(x, y, width, 16);
  const safe = hp < 0 ? 0 : hp;
  const filled = max <= 0 ? 0 : ((safe * width) / max) | 0;
  ctx.fillStyle = color;
  ctx.fillRect(x, y, filled, 16);
}

function padName(name: string, n: number): string {
  return name.length >= n ? name.slice(0, n) : name.padEnd(n, " ");
}

export { BG, OURS, THEIRS };
