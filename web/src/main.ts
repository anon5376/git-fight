import init, {
  WasmFight,
  demo_conflict,
  demo_resolve,
  ticks_per_second,
} from "../pkg/git_fight_wasm.js";
import { bindKeys } from "./input";
import { drawDotTitle } from "./render";
import {
  hostMatch,
  paintFight,
  routeFromPath,
  showKo,
  startOnline,
  startReplay,
} from "./online";

type Mode = { kind: "cpu"; human: "ours" | "theirs" } | { kind: "two" };

const smoke = new URLSearchParams(window.location.search).has("smoke");

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) {
    throw new Error(`missing #${id}`);
  }
  return el;
}

function show(id: string, on: boolean): void {
  $(id).classList.toggle("hidden", !on);
}

function makeFight(): WasmFight {
  if (smoke) {
    return WasmFight.with_hp(1, 100, 10);
  }
  return new WasmFight((Date.now() & 0xffff_ffff) >>> 0);
}

let running: { stop: () => void } | null = null;

function startFight(mode: Mode, fromDemo: boolean): void {
  running?.stop();
  show("menu", false);
  show("demo-panel", false);
  show("side-pick", false);
  show("arena", true);
  $("wait").classList.add("hidden");
  $("share").classList.add("hidden");
  const ko = $("ko");
  ko.classList.add("hidden");
  ko.textContent = "KO";
  $("resolved").textContent = "";

  const fight = makeFight();
  const stage = $("stage") as HTMLCanvasElement;
  const tps = ticks_per_second();
  const tickMs = 1000 / tps;
  let last = performance.now();
  let leftover = 0;
  let stopped = false;
  let finished = false;

  const keys = bindKeys(() => {
    stopped = true;
    showMenu();
  });

  const stepOnce = () => {
    if (fight.result() !== -1) {
      return;
    }
    const queued = keys.poll();
    let ours = queued.ours;
    let theirs = queued.theirs;
    if (mode.kind === "cpu") {
      if (mode.human === "ours") {
        // `?smoke=1` stands the CPU still so a mashed punch can KO the dummy HP.
        theirs = smoke ? 0 : fight.cpu_input(1);
      } else {
        theirs = ours;
        ours = smoke ? 0 : fight.cpu_input(0);
      }
    }
    fight.step(ours, theirs);
  };

  const finish = () => {
    if (finished) {
      return;
    }
    finished = true;
    const result = fight.result();
    showKo(ko, result);
    if (fromDemo) {
      if (result === 0) {
        $("resolved").textContent = demo_resolve(0);
      } else if (result === 1) {
        $("resolved").textContent = demo_resolve(1);
      } else {
        $("resolved").textContent = "draw — conflict left unresolved";
      }
    }
  };

  const loop = (now: number) => {
    if (stopped) {
      return;
    }
    leftover += now - last;
    last = now;
    while (leftover >= tickMs) {
      leftover -= tickMs;
      stepOnce();
    }
    const youSide = mode.kind === "two" ? "both" : mode.human;
    paintFight(stage, fight, "ours", "theirs", fromDemo ? "demo  1/1" : "round 1/1", undefined, undefined, youSide);
    if (fight.result() !== -1) {
      finish();
    }
    requestAnimationFrame(loop);
  };

  const youSide = mode.kind === "two" ? "both" : mode.human;
  paintFight(stage, fight, "ours", "theirs", fromDemo ? "demo  1/1" : "round 1/1", undefined, undefined, youSide);
  requestAnimationFrame(loop);
  running = {
    stop: () => {
      stopped = true;
      keys.unbind();
    },
  };
}

function showMenu(): void {
  running?.stop();
  running = null;
  show("menu", true);
  show("demo-panel", false);
  show("side-pick", false);
  show("arena", false);
}

function showDemo(): void {
  running?.stop();
  show("menu", false);
  show("side-pick", false);
  show("arena", false);
  show("demo-panel", true);
  const pre = document.querySelector("[data-testid=demo-conflict]");
  if (pre) {
    pre.textContent = demo_conflict();
  }
}

function showSidePick(): void {
  show("menu", false);
  show("demo-panel", false);
  show("arena", false);
  show("side-pick", true);
}

function arenaUi() {
  return {
    stage: $("stage") as HTMLCanvasElement,
    ko: $("ko"),
    wait: $("wait"),
    share: $("share"),
    resolved: $("resolved"),
    onQuit: () => {
      window.location.assign("/");
    },
  };
}

async function bootOnline(): Promise<boolean> {
  const route = routeFromPath();
  if (!route) {
    return false;
  }
  show("menu", false);
  show("demo-panel", false);
  show("side-pick", false);
  show("arena", true);
  $("resolved").textContent = "";
  if (route.kind === "match") {
    running = startOnline(route.id, route.token, arenaUi());
  } else {
    running = await startReplay(route.id, arenaUi());
  }
  return true;
}

async function main(): Promise<void> {
  await init();
  drawDotTitle($("title") as HTMLCanvasElement, "GIT FIGHT");
  if (await bootOnline()) {
    return;
  }
  // Menu starts hidden so vs-CPU is not clickable before wasm and listeners exist.
  document.querySelector("[data-testid=demo]")?.addEventListener("click", showDemo);
  document.querySelector("[data-testid=cpu]")?.addEventListener("click", showSidePick);
  document.querySelector("[data-testid=two]")?.addEventListener("click", () => {
    startFight({ kind: "two" }, false);
  });
  document.querySelector("[data-testid=host]")?.addEventListener("click", () => {
    void hostMatch().catch((err: unknown) => {
      const flash = $("flash");
      flash.classList.remove("hidden");
      flash.textContent = err instanceof Error ? err.message : "server is not running";
    });
  });
  document.querySelector("[data-testid=demo-fight]")?.addEventListener("click", () => {
    startFight({ kind: "cpu", human: "ours" }, true);
  });
  document.querySelector("[data-testid=play-ours]")?.addEventListener("click", () => {
    startFight({ kind: "cpu", human: "ours" }, false);
  });
  document.querySelector("[data-testid=play-theirs]")?.addEventListener("click", () => {
    startFight({ kind: "cpu", human: "theirs" }, false);
  });
  document.querySelector("[data-testid=back-demo]")?.addEventListener("click", showMenu);
  document.querySelector("[data-testid=back-side]")?.addEventListener("click", showMenu);
  showMenu();
}

void main();
