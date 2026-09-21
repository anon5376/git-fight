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
  you_are?: string;
  ours: string;
  theirs: string;
  round: number;
  total_rounds?: number;
  confirmed_tick: number;
  ours_hp?: number;
  ours_armor?: boolean;
  ours_special?: boolean;
  theirs_hp?: number;
  theirs_armor?: boolean;
  theirs_special?: boolean;
};

type TickMsg = { type: "tick"; n: number; ours: number; theirs: number };
type HashMsg = { type: "hash"; n: number; hi: number; lo: number };
type SnapshotMsg = {
  type: "snapshot";
  seed_lo: number;
  seed_hi: number;
  round: number;
  confirmed_tick: number;
  ours_hp?: number;
  ours_armor?: boolean;
  ours_special?: boolean;
  theirs_hp?: number;
  theirs_armor?: boolean;
  theirs_special?: boolean;
  ticks: number[][];
};
type EndMsg = {
  type: "end";
  result: number;
  hash_hi: number;
  hash_lo: number;
  tick: number;
  round?: number;
  match_over?: boolean;
};
type ErrMsg = { type: "error"; message: string };
type ServerMsg = Hello | TickMsg | HashMsg | SnapshotMsg | EndMsg | ErrMsg | { type: string };

function u32(n: number): number {
  return n >>> 0;
}

function hashesMatch(fight: WasmFight, hi: number, lo: number): boolean {
  return u32(fight.state_hash_hi()) === u32(hi) && u32(fight.state_hash_lo()) === u32(lo);
}

function fightFromWire(
  seedLo: number,
  seedHi: number,
  oursHp?: number,
  oursArmor?: boolean,
  oursSpecial?: boolean,
  theirsHp?: number,
  theirsArmor?: boolean,
  theirsSpecial?: boolean,
): WasmFight {
  return WasmFight.from_seed_stats(
    seedLo,
    seedHi,
    oursHp ?? 100,
    oursArmor ?? false,
    oursSpecial ?? false,
    theirsHp ?? 100,
    theirsArmor ?? false,
    theirsSpecial ?? false,
  );
}

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
  stage.dataset.round = String(roundLabel);
}

export function showKo(ko: HTMLElement, result: number): void {
  ko.classList.remove("hidden", "ours");
  if (result === 0) {
    ko.textContent = "KO";
    ko.classList.add("ours");
  } else if (result === 1) {
    ko.textContent = "KO";
  } else {
    ko.textContent = "DRAW";
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
  let finished = false;
  let round = 0;
  let totalRounds = 1;
  const tps = ticks_per_second();
  const tickMs = 1000 / tps;
  let last = performance.now();
  let leftover = 0;

  const applySnapshot = (snap: SnapshotMsg): void => {
    const rebuilt = fightFromWire(
      snap.seed_lo,
      snap.seed_hi,
      snap.ours_hp,
      snap.ours_armor,
      snap.ours_special,
      snap.theirs_hp,
      snap.theirs_armor,
      snap.theirs_special,
    );
    for (const pair of snap.ticks ?? []) {
      const n = pair[0] ?? 0;
      const oursBtn = pair[1] ?? 0;
      const theirsBtn = pair[2] ?? 0;
      if (n === rebuilt.tick()) {
        rebuilt.step(oursBtn, theirsBtn);
      }
    }
    fight = rebuilt;
    confirmed = snap.confirmed_tick;
    round = snap.round ?? round;
    nextSend = Math.max(0, confirmed + 1);
    if (rebuilt.tick() > 0 && role !== "spectator") {
      ui.wait.classList.add("hidden");
    }
  };

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

  let ws: WebSocket | null = null;
  let gen = 0;
  let reconnectTimer: number | null = null;
  let reconnectAttempts = 0;
  const maxReconnects = 8;

  const clearReconnect = () => {
    if (reconnectTimer !== null) {
      window.clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  };

  const flush = (oursBtn: number, theirsBtn: number) => {
    if (role === "spectator" || !ws || ws.readyState !== WebSocket.OPEN) {
      return;
    }
    const horizon = Math.max(0, confirmed + 1) + delay;
    while (nextSend <= horizon) {
      const buttons = nextSend === horizon ? oursBtn : 0;
      const theirs = nextSend === horizon ? theirsBtn : 0;
      const payload: { type: string; tick: number; buttons: number; theirs?: number } = {
        type: "input",
        tick: nextSend,
        buttons,
      };
      if (role === "both") {
        payload.theirs = theirs;
      }
      ws.send(JSON.stringify(payload));
      nextSend += 1;
    }
  };

  const scheduleReconnect = (why: string) => {
    if (stopped || finished) {
      return;
    }
    reconnectAttempts += 1;
    if (reconnectAttempts > maxReconnects) {
      ui.wait.classList.remove("hidden");
      ui.wait.textContent = "desync — reloading";
      window.location.reload();
      return;
    }
    ui.wait.classList.remove("hidden");
    ui.wait.textContent = why;
    clearReconnect();
    reconnectTimer = window.setTimeout(() => {
      openSocket();
    }, 400);
  };

  const onMessage = (ev: MessageEvent) => {
    const msg = JSON.parse(String(ev.data)) as ServerMsg;
    if (msg.type === "hello") {
      const hello = msg as Hello;
      role = hello.your_role;
      delay = hello.input_delay;
      confirmed = hello.confirmed_tick;
      oursName = hello.ours;
      theirsName = hello.theirs;
      round = hello.round ?? 0;
      totalRounds = hello.total_rounds ?? 1;
      nextSend = Math.max(0, confirmed + 1);
      finished = false;
      reconnectAttempts = 0;
      ui.ko.classList.add("hidden");
      fight = fightFromWire(
        hello.seed_lo,
        hello.seed_hi,
        hello.ours_hp,
        hello.ours_armor,
        hello.ours_special,
        hello.theirs_hp,
        hello.theirs_armor,
        hello.theirs_special,
      );
      if (role === "spectator") {
        ui.wait.classList.remove("hidden");
        if (!ui.wait.querySelector("[data-testid=\"github-login\"]")) {
          ui.wait.textContent = "spectating";
          void offerGithubLogin(matchId, ui);
        }
      } else {
        ui.wait.textContent = "waiting for opponent…";
      }
    } else if (msg.type === "tick") {
      const tick = msg as TickMsg;
      if (!fight) {
        return;
      }
      if (tick.n === fight.tick()) {
        fight.step(tick.ours, tick.theirs);
        confirmed = tick.n;
        nextSend = Math.max(nextSend, confirmed + 1);
        ui.wait.classList.add("hidden");
      }
    } else if (msg.type === "hash") {
      const hash = msg as HashMsg;
      if (!fight || finished) {
        return;
      }
      if (hash.n !== fight.tick() || !hashesMatch(fight, hash.hi, hash.lo)) {
        ui.wait.classList.remove("hidden");
        ui.wait.textContent = "desync — reconnecting";
        ws?.close();
      }
    } else if (msg.type === "snapshot") {
      const snap = msg as SnapshotMsg;
      const expect = snap.confirmed_tick < 0 ? 0 : snap.confirmed_tick + 1;
      if (!fight || fight.tick() !== expect) {
        applySnapshot(snap);
      }
    } else if (msg.type === "end") {
      const end = msg as EndMsg;
      const matchOver = end.match_over !== false;
      ui.wait.classList.add("hidden");
      showKo(ui.ko, end.result);
      if (matchOver) {
        finished = true;
      }
      if (fight && end.tick === fight.tick() && !hashesMatch(fight, end.hash_hi, end.hash_lo)) {
        ui.resolved.textContent = "desync — server result stands";
      } else if (matchOver) {
        ui.resolved.textContent = `replay /replay/${matchId}`;
      } else {
        ui.resolved.textContent = `round ${(end.round ?? round) + 1}/${totalRounds}`;
      }
    } else if (msg.type === "error") {
      const err = msg as ErrMsg;
      ui.wait.classList.remove("hidden");
      ui.wait.textContent = err.message;
    }
  };

  const openSocket = () => {
    if (stopped || finished) {
      return;
    }
    gen += 1;
    const myGen = gen;
    const socket = new WebSocket(wsUrl(matchId, token));
    ws = socket;
    socket.addEventListener("open", () => {
      if (myGen !== gen) {
        return;
      }
      ui.wait.textContent = "waiting for opponent…";
    });
    socket.addEventListener("close", () => {
      if (myGen !== gen) {
        return;
      }
      if (!finished && !stopped) {
        scheduleReconnect("disconnected — reconnecting");
      }
    });
    socket.addEventListener("message", (ev) => {
      if (myGen !== gen) {
        return;
      }
      onMessage(ev);
    });
  };

  openSocket();

  const loop = (now: number) => {
    if (stopped) {
      return;
    }
    leftover += now - last;
    last = now;
    while (leftover >= tickMs) {
      leftover -= tickMs;
      const queued = keys.poll();
      if (role === "both") {
        const horizon = Math.max(0, confirmed + 1) + delay;
        if (nextSend <= horizon) {
          flush(queued.ours || held, queued.theirs);
          held = 0;
        } else {
          held = queued.ours || held;
        }
      } else {
        const latest = buttonsForRole(role, queued) || held;
        if (role !== "spectator") {
          const horizon = Math.max(0, confirmed + 1) + delay;
          if (nextSend <= horizon) {
            flush(latest, 0);
            held = 0;
          } else {
            held = latest;
          }
        }
      }
    }
    if (fight) {
      paintFight(
        ui.stage,
        fight,
        oursName,
        theirsName,
        `online ${round + 1}/${totalRounds}`,
      );
    }
    requestAnimationFrame(loop);
  };
  requestAnimationFrame(loop);

  return {
    stop: () => {
      stopped = true;
      clearReconnect();
      gen += 1;
      keys.unbind();
      if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
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
    ticks: number[][];
    final_hash?: string;
    ours_hp?: number;
    ours_armor?: boolean;
    ours_special?: boolean;
    theirs_hp?: number;
    theirs_armor?: boolean;
    theirs_special?: boolean;
    rounds?: Array<{
      round: number;
      seed_lo: number;
      seed_hi: number;
      ticks: number[][];
      ours_hp?: number;
      ours_armor?: boolean;
      ours_special?: boolean;
      theirs_hp?: number;
      theirs_armor?: boolean;
      theirs_special?: boolean;
    }>;
  };
  const rounds =
    data.rounds && data.rounds.length > 0
      ? data.rounds
      : [
          {
            round: 0,
            seed_lo: Number(BigInt(data.seed) & 0xffffffffn),
            seed_hi: Number(BigInt(data.seed) >> 32n),
            ticks: data.ticks ?? [],
            ours_hp: data.ours_hp,
            ours_armor: data.ours_armor,
            ours_special: data.ours_special,
            theirs_hp: data.theirs_hp,
            theirs_armor: data.theirs_armor,
            theirs_special: data.theirs_special,
          },
        ];
  const makeFight = (round: (typeof rounds)[0]): WasmFight =>
    fightFromWire(
      round.seed_lo,
      round.seed_hi,
      round.ours_hp,
      round.ours_armor,
      round.ours_special,
      round.theirs_hp,
      round.theirs_armor,
      round.theirs_special,
    );
  let ri = 0;
  let fight = makeFight(rounds[0]);
  let ticks = rounds[0]?.ticks ?? [];
  let i = 0;
  let stopped = false;
  let finished = false;
  let between = false;
  let hold = 0;
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
    while (leftover >= tickMs) {
      leftover -= tickMs;
      if (hold > 0) {
        hold -= 1;
        continue;
      }
      if (between) {
        between = false;
        ri += 1;
        const next = rounds[ri];
        fight = makeFight(next);
        ticks = next.ticks ?? [];
        i = 0;
        ui.ko.classList.add("hidden");
        continue;
      }
      if (i < ticks.length) {
        const pair = ticks[i] ?? [0, 0];
        fight.step(pair[0] ?? 0, pair[1] ?? 0);
        i += 1;
      } else if (ri + 1 < rounds.length) {
        const result = fight.result();
        if (result !== -1) {
          showKo(ui.ko, result);
        }
        hold = Math.max(1, Math.floor(tps / 2));
        between = true;
      } else if (!finished) {
        finished = true;
        const result = fight.result();
        if (result !== -1) {
          showKo(ui.ko, result);
        }
        ui.resolved.textContent = data.final_hash ? `hash ${data.final_hash}` : "";
      }
    }
    paintFight(ui.stage, fight, "ours", "theirs", `replay ${ri + 1}/${rounds.length}`);
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

async function offerGithubLogin(matchId: string, ui: OnlineUi): Promise<void> {
  try {
    const info = await fetch(`/api/matches/${matchId}`, { credentials: "include" });
    if (!info.ok) {
      return;
    }
    const body = (await info.json()) as { ours_login?: string; theirs_login?: string };
    if (!body.ours_login && !body.theirs_login) {
      return;
    }
    const me = await fetch("/api/me", { credentials: "include" });
    if (me.ok) {
      return;
    }
    ui.wait.textContent = "";
    const a = document.createElement("a");
    a.href = `/auth/github?return=/match/${matchId}`;
    a.textContent = "log in with GitHub to take a fighter slot";
    a.dataset.testid = "github-login";
    ui.wait.appendChild(a);
    ui.wait.classList.remove("hidden");
  } catch {
    /* offline */
  }
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
