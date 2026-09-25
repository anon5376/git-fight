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

/** Matches `git_fight_core::ARENA_W`. One unit is one sprite column. */
export const ARENA_UNITS = 72;

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

export type Playfield = {
  scale: number;
  originX: number;
  fieldW: number;
};

/** Fit the whole arena on the canvas. Fighters were drawn at 8px per unit, so on the
 *  880px stage they never reached the right health bar. */
export function layoutPlayfield(canvasWidth: number, arenaUnits = ARENA_UNITS): Playfield {
  const units = arenaUnits > 0 ? arenaUnits : ARENA_UNITS;
  const pad = 24;
  const scale = Math.max(4, Math.floor((canvasWidth - pad * 2) / units));
  const fieldW = units * scale;
  const originX = Math.floor((canvasWidth - fieldW) / 2);
  return { scale, originX, fieldW };
}

export function drawFrame(canvas: HTMLCanvasElement, frame: Frame): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    return;
  }
  const w = canvas.width;
  const h = canvas.height;
  ctx.imageSmoothingEnabled = false;
  ctx.fillStyle = BG;
  ctx.fillRect(0, 0, w, h);
  ctx.font = '18px ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace';
  ctx.textBaseline = "top";
  ctx.textAlign = "left";

  const barW = Math.min(280, Math.max(80, Math.floor((w - 160) / 2)));
  const rightBar = w - 24 - barW;

  ctx.fillStyle = OURS;
  ctx.fillText("GIT FIGHT", 24, 16);
  ctx.textAlign = "center";
  ctx.fillText(frame.roundLabel, Math.floor(w / 2), 16);
  ctx.textAlign = "right";
  ctx.fillText(frame.timer, w - 24, 16);
  ctx.textAlign = "left";
  ctx.fillText(clipName(frame.oursName, 12), 24, 48);
  ctx.textAlign = "right";
  ctx.fillText(clipName(frame.theirsName, 12), w - 24, 48);
  ctx.textAlign = "left";

  drawBar(ctx, 24, 72, barW, frame.oursHp, frame.oursMax, OURS);
  drawBar(ctx, rightBar, 72, barW, frame.theirsHp, frame.theirsMax, THEIRS);
  ctx.fillStyle = OURS;
  ctx.fillText(String(frame.oursHp).padStart(3, " "), 24 + barW + 8, 70);
  ctx.fillStyle = THEIRS;
  ctx.textAlign = "right";
  ctx.fillText(String(frame.theirsHp).padStart(3, " "), rightBar - 8, 70);
  ctx.textAlign = "left";

  const field = layoutPlayfield(w);
  const originY = Math.max(120, Math.floor(h * 0.33));
  drawSprite(ctx, frame.oursSprite, field.originX + frame.oursX * field.scale, originY, OURS, field.scale);
  drawSprite(ctx, frame.theirsSprite, field.originX + frame.theirsX * field.scale, originY, THEIRS, field.scale);

  const groundY = originY + 5 * field.scale + 8;
  ctx.fillStyle = "#333";
  ctx.fillRect(field.originX, groundY, field.fieldW, 2);
  ctx.fillStyle = OURS;
  ctx.font = '14px ui-monospace, "Cascadia Code", "SF Mono", Menlo, monospace';
  ctx.fillText("a punch  s kick  d block  f special     j k l ;     q menu", 24, groundY + 16);
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
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  for (let r = 0; r < rows.length; r += 1) {
    const line = rows[r] ?? "";
    for (let c = 0; c < line.length; c += 1) {
      const ch = line[c];
      if (ch === " ") {
        continue;
      }
      ctx.fillText(ch, x + c * cell + cell / 2, y + r * cell + cell / 2);
    }
  }
  ctx.textAlign = "left";
  ctx.textBaseline = "top";
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

function clipName(name: string, n: number): string {
  return name.length >= n ? name.slice(0, n) : name;
}

export { BG, OURS, THEIRS };
