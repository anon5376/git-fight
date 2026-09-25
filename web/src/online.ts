import {
  WasmFight,
  round_ticks,
  sprite_row,
  sprite_rows,
  ticks_per_second,
} from "../pkg/git_fight_wasm.js";
import { bindKeys, buttonsForRole } from "./input";
import { drawFrame } from "./render";

export type OnlineUi = {
  stage: HTMLCanvasElement;
  ko: HTMLElement;
  wait: HTMLElement;
  share: HTMLElement;
  resolved: HTMLElement;
  onQuit: () => void;
};

type Hello = {
  type: "hello";
  match_id: string;
  seed_lo: number;
  seed_hi: number;
  input_delay: number;
  your_role: string;
  ours: string;
  theirs: string;
  round: number;
  confirmed_tick: number;
};

type TickMsg = { type: "tick"; n: number; ours: number; theirs: number };
type EndMsg = {
  type: "end";
  result: number;
  hash_hi: number;
  hash_lo: number;
  tick: number;
};
type ErrMsg = { type: "error"; message: string };
type ServerMsg = Hello | TickMsg | EndMsg | ErrMsg | { type: string };

export function sprite(side: number, pose: number): string[] {
  const rows = sprite_rows();
  const out: string[] = [];
  for (let r = 0; r < rows; r += 1) {
    out.push(sprite_row(side, pose, r));
  }
  return out;
}

export function paintFight(
  stage: HTMLCanvasElement,
  fight: WasmFight,
  oursName: string,
  theirsName: string,
  roundLabel: string,
): void {
  const tps = ticks_per_second();
  const round = round_ticks();
  const left = round - fight.tick();
  const secs = tps === 0 ? 0 : (left - (left % tps)) / tps;
  drawFrame(stage, {
    oursName,
    theirsName,
    oursHp: fight.ours_hp(),
    theirsHp: fight.theirs_hp(),
    oursMax: fight.ours_max_hp(),
    theirsMax: fight.theirs_max_hp(),
    oursX: fight.ours_x(),
    theirsX: fight.theirs_x(),
    oursSprite: sprite(0, fight.ours_pose()),
    theirsSprite: sprite(1, fight.theirs_pose()),
    timer: String(secs).padStart(2, " "),
    roundLabel,
  });
  stage.dataset.oursHp = String(fight.ours_hp());
  stage.dataset.theirsHp = String(fight.theirs_hp());
}

export function showKo(ko: HTMLElement, result: number, decision = false): void {
  ko.classList.remove("hidden", "ours");
  if (result === 2) {
    ko.textContent = "DRAW";
    return;
  }
  if (decision) {
    ko.textContent = "TIME";
  } else {
    ko.textContent = "KO";
  }
  if (result === 0) {
    ko.classList.add("ours");
  }
}

function wsUrl(matchId: string, token: string | null): string {
  const proto = window.location.protocol === "https:" ? "wss" : "ws";
  const url = new URL(`${proto}://${window.location.host}/ws`);
  url.searchParams.set("match", matchId);
  if (token) {
    url.searchParams.set("token", token);
  }
  return url.toString();
}

export function startOnline(matchId: string, token: string | null, ui: OnlineUi): { stop: () => void } {
  let stopped = false;
  let fight: WasmFight | null = null;
  let role = "spectator";
  let delay = 3;
  let nextSend = 0;
  let confirmed = -1;
  let held = 0;
  let oursName = "ours";
  let theirsName = "theirs";
  let roundLabel = "online 1/1";
  let finished = false;
  const tps = ticks_per_second();
  const tickMs = 1000 / tps;
  let last = performance.now();
  let leftover = 0;

  ui.wait.classList.remove("hidden");
  ui.wait.textContent = "connecting…";
  ui.ko.classList.add("hidden");
  const share = sessionStorage.getItem("git-fight-share");
  if (share) {
    ui.share.classList.remove("hidden");
    ui.share.textContent = `opponent link: ${share}`;
  } else {
    ui.share.classList.add("hidden");
  }

  const keys = bindKeys(() => {
    stopped = true;
    ui.onQuit();
  });

  const ws = new WebSocket(wsUrl(matchId, token));

  const flush = (latest: number) => {
    if (role === "spectator" || ws.readyState !== WebSocket.OPEN) {
      return;
    }
    const horizon = Math.max(0, confirmed + 1) + delay;
    while (nextSend <= horizon) {
      const buttons = nextSend === horizon ? latest : 0;
      ws.send(JSON.stringify({ type: "input", tick: nextSend, buttons }));
      nextSend += 1;
    }
  };

  ws.addEventListener("open", () => {
    ui.wait.textContent = "waiting for opponent…";
  });
  ws.addEventListener("close", () => {
    if (!finished && !stopped) {
      ui.wait.classList.remove("hidden");
      ui.wait.textContent = "disconnected — reconnecting in 2s";
      window.setTimeout(() => {
        if (!stopped && !finished) {
          window.location.reload();
        }
      }, 2000);
    }
  });
  ws.addEventListener("message", (ev) => {
    const msg = JSON.parse(String(ev.data)) as ServerMsg;
    if (msg.type === "hello") {
      const hello = msg as Hello;
      role = hello.your_role;
      delay = hello.input_delay;
      confirmed = hello.confirmed_tick;
      oursName = hello.ours;
      theirsName = hello.theirs;
      roundLabel = `online ${hello.round + 1}/1`;
      fight = WasmFight.from_seed(hello.seed_lo, hello.seed_hi);
      if (role === "spectator") {
        ui.wait.textContent = "spectating";
      }
    } else if (msg.type === "tick") {
      const tick = msg as TickMsg;
      if (!fight) {
        return;
      }
      if (tick.n === fight.tick()) {
        fight.step(tick.ours, tick.theirs);
        confirmed = tick.n;
        ui.wait.classList.add("hidden");
      }
    } else if (msg.type === "end") {
      const end = msg as EndMsg;
      finished = true;
      ui.wait.classList.add("hidden");
      if (fight && fight.result() === -1) {
        if (end.result === 0) {
          fight.forfeit(1);
        } else if (end.result === 1) {
          fight.forfeit(0);
        }
      }
      const decision = fight ? fight.ours_hp() > 0 && fight.theirs_hp() > 0 : false;
      showKo(ui.ko, end.result, decision);
      ui.resolved.textContent = `replay /replay/${matchId}`;
    } else if (msg.type === "error") {
      const err = msg as ErrMsg;
      ui.wait.classList.remove("hidden");
      ui.wait.textContent = err.message;
    }
  });

  const loop = (now: number) => {
    if (stopped) {
      return;
    }
    leftover += now - last;
    last = now;
    let steps = 0;
    while (leftover >= tickMs && steps < 3) {
      leftover -= tickMs;
      steps += 1;
      const queued = keys.poll();
      const latest = buttonsForRole(role, queued) || held;
      if (role !== "spectator") {
        const horizon = Math.max(0, confirmed + 1) + delay;
        if (nextSend <= horizon) {
          flush(latest);
          held = 0;
        } else {
          held = latest;
        }
      }
    }
    if (leftover > tickMs) {
      leftover = 0;
    }
    if (fight) {
      paintFight(ui.stage, fight, oursName, theirsName, roundLabel);
    }
    requestAnimationFrame(loop);
  };
  requestAnimationFrame(loop);

  return {
    stop: () => {
      stopped = true;
      keys.unbind();
      if (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING) {
        ws.close();
      }
    },
  };
}

export async function startReplay(matchId: string, ui: OnlineUi): Promise<{ stop: () => void }> {
  ui.wait.classList.remove("hidden");
  ui.wait.textContent = "loading replay…";
  ui.share.classList.add("hidden");
  ui.ko.classList.add("hidden");
  const res = await fetch(`/api/replays/${matchId}`);
  if (!res.ok) {
    ui.wait.textContent = "replay not found";
    return { stop: () => undefined };
  }
  const data = (await res.json()) as {
    seed: string;
    ours?: string;
    theirs?: string;
    ticks: number[][];
    final_hash?: string;
  };
  const seed = BigInt(data.seed);
  const seedLo = Number(seed & 0xffffffffn);
  const seedHi = Number(seed >> 32n);
  const fight = WasmFight.from_seed(seedLo, seedHi);
  const ticks = data.ticks ?? [];
  let i = 0;
  let stopped = false;
  let finished = false;
  const tps = ticks_per_second();
  const tickMs = 1000 / tps;
  let last = performance.now();
  let leftover = 0;
  ui.wait.classList.add("hidden");

  const keys = bindKeys(() => {
    stopped = true;
    ui.onQuit();
  });

  const loop = (now: number) => {
    if (stopped) {
      return;
    }
    leftover += now - last;
    last = now;
    let steps = 0;
    while (leftover >= tickMs && steps < 3) {
      leftover -= tickMs;
      steps += 1;
      if (i < ticks.length) {
        const pair = ticks[i] ?? [0, 0];
        fight.step(pair[0] ?? 0, pair[1] ?? 0);
        i += 1;
      } else if (!finished) {
        finished = true;
        const result = fight.result();
        if (result !== -1) {
          const decision = fight.ours_hp() > 0 && fight.theirs_hp() > 0;
          showKo(ui.ko, result, decision);
        }
        ui.resolved.textContent = data.final_hash ? `hash ${data.final_hash}` : "";
      }
    }
    if (leftover > tickMs) {
      leftover = 0;
    }
    paintFight(ui.stage, fight, data.ours || "ours", data.theirs || "theirs", "replay 1/1");
    requestAnimationFrame(loop);
  };
  requestAnimationFrame(loop);
  return {
    stop: () => {
      stopped = true;
      keys.unbind();
    },
  };
}

export async function hostMatch(): Promise<void> {
  const res = await fetch("/api/matches", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  if (!res.ok) {
    throw new Error("server is not running");
  }
  const created = (await res.json()) as {
    id: string;
    ours_token: string;
    theirs_token: string;
  };
  const theirs = `${window.location.origin}/match/${created.id}?token=${created.theirs_token}`;
  sessionStorage.setItem("git-fight-share", theirs);
  window.location.assign(`/match/${created.id}?token=${created.ours_token}`);
}

export function routeFromPath():
  | { kind: "match"; id: string; token: string | null }
  | { kind: "replay"; id: string }
  | null {
  const parts = window.location.pathname.split("/").filter(Boolean);
  const token = new URLSearchParams(window.location.search).get("token");
  if (parts[0] === "match" && parts[1]) {
    return { kind: "match", id: parts[1], token };
  }
  if (parts[0] === "replay" && parts[1]) {
    return { kind: "replay", id: parts[1] };
  }
  return null;
}
