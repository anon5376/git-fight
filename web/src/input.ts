export type Queued = { ours: number; theirs: number };

export function bindKeys(onQuit: () => void): { poll: () => Queued; unbind: () => void } {
  let ours = 0;
  let theirs = 0;
  const down = (ev: KeyboardEvent) => {
    if (ev.repeat) {
      return;
    }
    if (ev.key === "q" || ev.key === "Q") {
      ev.preventDefault();
      onQuit();
      return;
    }
    const mapped = mapKey(ev.key);
    if (!mapped) {
      return;
    }
    ev.preventDefault();
    if (mapped.ours !== 0) {
      ours = mapped.ours;
    }
    if (mapped.theirs !== 0) {
      theirs = mapped.theirs;
    }
  };
  window.addEventListener("keydown", down, true);
  return {
    poll: () => {
      const out = { ours, theirs };
      ours = 0;
      theirs = 0;
      return out;
    },
    unbind: () => window.removeEventListener("keydown", down, true),
  };
}

/** Fighter slots use P1 keys; P2 keys still work for the right side. */
export function buttonsForRole(role: string, queued: Queued): number {
  if (role === "theirs") {
    return queued.theirs !== 0 ? queued.theirs : queued.ours;
  }
  if (role === "ours" || role === "both") {
    return queued.ours;
  }
  return 0;
}

function mapKey(key: string): Queued | null {
  switch (key) {
    case "a":
    case "A":
      return { ours: 1, theirs: 0 };
    case "s":
    case "S":
      return { ours: 2, theirs: 0 };
    case "d":
    case "D":
      return { ours: 3, theirs: 0 };
    case "f":
    case "F":
      return { ours: 4, theirs: 0 };
    case "j":
    case "J":
      return { ours: 0, theirs: 1 };
    case "k":
    case "K":
      return { ours: 0, theirs: 2 };
    case "l":
    case "L":
      return { ours: 0, theirs: 3 };
    case ";":
      return { ours: 0, theirs: 4 };
    default:
      return null;
  }
}
