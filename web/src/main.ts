import init, {
  WasmFight,
  demo_conflict,
  demo_resolve,
  round_ticks,
  sprite_row,
  sprite_rows,
  ticks_per_second,
} from "../pkg/git_fight_wasm.js";
import { bindKeys } from "./input";
import { drawDotTitle, drawFrame } from "./render";

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

function sprite(side: number, pose: number): string[] {
  const rows = sprite_rows();
  const out: string[] = [];
  for (let r = 0; r < rows; r += 1) {
    out.push(sprite_row(side, pose, r));
  }
  return out;
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
  const ko = $("ko");
  ko.classList.add("hidden");
  ko.textContent = "KO";
  $("resolved").textContent = "";

  const fight = makeFight();
  const stage = $("stage") as HTMLCanvasElement;
  const tps = ticks_per_second();
  const round = round_ticks();
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
        theirs = fight.cpu_input(1);
      } else {
        theirs = ours;
        ours = fight.cpu_input(0);
      }
    }
    fight.step(ours, theirs);
  };

  const paint = () => {
    const left = round - fight.tick();
            const secs = tps === 0 ? 0 : (left - (left % tps)) / tps;
    drawFrame(stage, {
      oursName: "ours",
      theirsName: "theirs",
      oursHp: fight.ours_hp(),
      theirsHp: fight.theirs_hp(),
      oursMax: fight.ours_max_hp(),
      theirsMax: fight.theirs_max_hp(),
      oursX: fight.ours_x(),
      theirsX: fight.theirs_x(),
      oursSprite: sprite(0, fight.ours_pose()),
      theirsSprite: sprite(1, fight.theirs_pose()),
      timer: String(secs).padStart(2, " "),
      roundLabel: fromDemo ? "demo  1/1" : "round 1/1",
    });
    stage.dataset.oursHp = String(fight.ours_hp());
    stage.dataset.theirsHp = String(fight.theirs_hp());
  };

  const finish = () => {
    if (finished) {
      return;
    }
    finished = true;
    const result = fight.result();
    ko.classList.remove("hidden", "ours");
    if (result === 0) {
      ko.textContent = "KO";
      ko.classList.add("ours");
      if (fromDemo) {
        $("resolved").textContent = demo_resolve(0);
      }
    } else if (result === 1) {
      ko.textContent = "KO";
      if (fromDemo) {
        $("resolved").textContent = demo_resolve(1);
      }
    } else {
      ko.textContent = "DRAW";
      if (fromDemo) {
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
    paint();
    if (fight.result() !== -1) {
      finish();
    }
    requestAnimationFrame(loop);
  };

  paint();
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

async function main(): Promise<void> {
  await init();
  drawDotTitle($("title") as HTMLCanvasElement, "GIT FIGHT");
  document.querySelector("[data-testid=demo]")?.addEventListener("click", showDemo);
  document.querySelector("[data-testid=cpu]")?.addEventListener("click", showSidePick);
  document.querySelector("[data-testid=two]")?.addEventListener("click", () => {
    startFight({ kind: "two" }, false);
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
}

void main();
