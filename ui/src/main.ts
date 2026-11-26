import { api as apiPromise, type Details, type Index, type Item, type State, type Stats, type Status } from "./api";
import { Orb, type OrbTarget } from "./orb";

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

const api = await apiPromise;
const os = await api.platform();
document.body.classList.add(`os-${os}`);
const android = os === "android";

// Phones: the layout was drawn for a 380 px desktop window; zoom it to the
// screen. Safe-area insets are measured unzoomed and handed to CSS divided by
// the zoom, so they land exactly on the system bars.
function fitPhone() {
  const root = document.documentElement;
  const body = document.body;
  body.style.zoom = "1";
  const probe = document.createElement("div");
  probe.style.cssText =
    "position:fixed;left:0;top:0;width:1px;visibility:hidden;pointer-events:none;" +
    "height:env(safe-area-inset-top,0px);padding-bottom:env(safe-area-inset-bottom,0px)";
  root.append(probe);
  const bottom = parseFloat(getComputedStyle(probe).paddingBottom || "0");
  const top = probe.getBoundingClientRect().height - bottom;
  probe.remove();
  const w = root.clientWidth;
  const h = root.clientHeight;
  const k = Math.min(1.3, Math.max(1, w / 330));
  root.style.setProperty("--k", String(k));
  root.style.setProperty("--sa-top", `${top / k}px`);
  root.style.setProperty("--sa-bottom", `${bottom / k}px`);
  // the zoomed page must still fit the screen exactly
  body.style.width = `${w / k}px`;
  body.style.height = `${h / k}px`;
  body.style.zoom = String(k);
}
// Touch screens keep :hover on whatever was tapped last (a button stays lit
// after its panel closed): phones get the stylesheet without hover states.
function dropHover(group: CSSStyleSheet | CSSGroupingRule) {
  const rules = group.cssRules;
  for (let i = rules.length - 1; i >= 0; i--) {
    const r = rules[i];
    if (r instanceof CSSStyleRule) {
      if (!r.selectorText.includes(":hover")) continue;
      const keep = r.selectorText.split(",").filter((sel) => !sel.includes(":hover"));
      if (keep.length) r.selectorText = keep.join(",");
      else group.deleteRule(i);
    } else if (r instanceof CSSGroupingRule) {
      dropHover(r);
    }
  }
}

if (android) {
  for (const sheet of document.styleSheets) {
    try {
      dropHover(sheet);
    } catch {
      // a sheet we can't read has no hover rules of ours
    }
  }
  fitPhone();
  let fitTimer = 0;
  // rotation, the keyboard opening
  addEventListener("resize", () => {
    clearTimeout(fitTimer);
    fitTimer = window.setTimeout(fitPhone, 60);
  });
}

// ---------- dev hooks (stripped from production builds) ----------

const dev = import.meta.env.DEV ? new URLSearchParams(location.hash.slice(1)) : null;
// screenshots: headless Chrome can't make a 380 px window
if (dev?.has("frame")) document.documentElement.style.cssText = "width:380px;height:600px";
const canvas = $<HTMLCanvasElement>("orb");
const power = $<HTMLButtonElement>("power");
// phones: the soft orb looks the same at 1.5x and leaves the GPU to the animations;
// its slow swirl needs no more than 30 fps (battery)
if (android) {
  Orb.maxDpr = 1.5;
  Orb.awakeFps = 32;
}
// `t=<seconds>` pins the shader clock so a screenshot can be reproduced
const orb = dev?.has("nogl") ? null : Orb.create(canvas, dev?.has("t") ? Number(dev.get("t")) : undefined);
if (!orb) document.body.classList.add("no-gl");

let index: Index = await api.list();

// ---------- appearance: the saved choice, or the system's; `#light` / `#dark` force it in dev ----------

const lightQuery = matchMedia("(prefers-color-scheme: light)");
const isLight = () =>
  dev?.has("light") ? true : dev?.has("dark") ? false : index.theme === "system" ? lightQuery.matches : index.theme === "light";
function applyTheme(instant: boolean) {
  document.documentElement.classList.toggle("light", isLight());
  orb?.theme(isLight(), instant);
}
applyTheme(true);
lightQuery.addEventListener("change", () => applyTheme(false));
let status: Status = await api.status();
let busy = false;
let prev: { t: number; rx: number; tx: number } | null = null;

// ---------- orb + readout ----------

const ORB: Record<State, OrbTarget> = {
  off: { energy: 0.28, spin: 0, warm: 0 },
  connecting: { energy: 0.42, spin: 1, warm: 0 },
  connected: { energy: 1, spin: 0, warm: 0 },
  unreachable: { energy: 0.36, spin: 0, warm: 1 },
  stalled: { energy: 0.5, spin: 1, warm: 1 },
};

const selected = () => index.items.find((i) => i.id === index.selected) ?? null;
// while a tunnel is up (or coming up) show the server it actually uses; otherwise the chosen one
const active = () => (status.state !== "off" && index.items.find((i) => i.id === status.configId)) || selected();

function elapsed(since: number) {
  const s = Math.max(0, Math.floor((Date.now() - since) / 1000));
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(Math.floor(s / 3600))}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}`;
}

function rate(bps: number): [string, string] {
  if (bps < 1000) return [String(Math.round(bps)), "B/s"];
  if (bps < 1e6) return [(bps / 1e3).toFixed(bps < 1e4 ? 1 : 0), "KB/s"];
  return [(bps / 1e6).toFixed(bps < 1e8 ? 1 : 0), "MB/s"];
}

function showRate(id: "down" | "up", bps: number) {
  const [v, u] = rate(bps);
  $(id).textContent = v;
  $(`${id}Unit`).textContent = u;
}

/** Two-letter monogram for the server badge: "frankfurt" → "FR". */
function mono(it: Item | null) {
  const s = (it?.name ?? "").replace(/[^\p{L}\p{N}]/gu, "");
  return s.slice(0, 2).toUpperCase();
}

/** Capitalised, with a full stop: backend messages arrive as bare clauses. */
const sentence = (s: string) => {
  const t = s.trim();
  return t.charAt(0).toUpperCase() + t.slice(1) + (/[.!?)]$/.test(t) ? "" : ".");
};
/** An error from the backend, as a sentence for people. */
const errText = (e: unknown) => sentence(String(e).replace(/^Error:\s*/, ""));

let first = true;
function renderStatus() {
  const s = status.state;
  const empty = !index.items.length;
  const failed = s === "off" && !!status.error;
  const body = document.body;
  body.dataset.state = s;
  body.classList.toggle("empty", empty);

  // a failed attempt leaves the sleeping ring faintly warm
  const look = failed ? { energy: 0.22, spin: 0, warm: 0.85 } : ORB[s];
  if (first) orb?.snap(look);
  else orb?.set(look);
  first = false;

  const name = active()?.name ?? "the server";
  const [title, sub] = ((): [string, string] => {
    switch (s) {
      case "connecting":
        return ["Connecting", `Masking the handshake to ${name}`];
      case "connected":
        return ["Connected", status.since ? elapsed(status.since) : "00:00:00"];
      case "unreachable":
        return ["No answer", "This server's IP looks blocked here.\nTry another one."];
      case "stalled":
        return ["Reconnecting", "Traffic was throttled.\nSwitching port and masking harder."];
      default:
        if (failed) return ["Couldn't connect", sentence(status.error!)];
        if (empty)
          return android
            ? ["Add a server", "Tap the orb to add a WireGuard config:\na file, a QR code or the clipboard"]
            : ["Add a server", "Drop a WireGuard .conf or QR picture\nanywhere, or paste a config"];
        return ["Not connected", "Tap the orb to connect"];
    }
  })();
  const titleEl = $("title");
  const subEl = $("sub");
  if (titleEl.textContent !== title) {
    titleEl.textContent = title;
    // new words rise into place rather than popping
    const readout = titleEl.parentElement!;
    readout.classList.remove("swap");
    void readout.offsetWidth;
    readout.classList.add("swap");
  }
  subEl.textContent = sub;
  subEl.className = s === "connected" ? "timer" : failed ? "err" : "";
  // the ticking timer must not be read out every second
  subEl.setAttribute("aria-live", s === "connected" ? "off" : "polite");
  power.setAttribute("aria-label", empty ? "Import a WireGuard config" : s === "off" ? "Connect" : "Disconnect");
  if (s !== "connected") {
    prev = null;
    showRate("down", 0);
    showRate("up", 0);
  }
  renderPicker();
}

function onStats(st: Stats) {
  // nobody's looking: skip the work, and start the rate afresh on return
  if (document.hidden) {
    prev = null;
    return;
  }
  const now = performance.now();
  if (prev) {
    const dt = (now - prev.t) / 1000;
    if (dt > 0.2) {
      showRate("down", Math.max(0, st.rx - prev.rx) / dt);
      showRate("up", Math.max(0, st.tx - prev.tx) / dt);
    }
  }
  prev = { t: now, rx: st.rx, tx: st.tx };
  lastStats = st;
  liveSheet();
}

setInterval(() => {
  if (status.state === "connected" && status.since && !document.hidden) $("sub").textContent = elapsed(status.since);
}, 1000);

// Android has no file type for .conf (they arrive as generic binary), so a
// filter would grey them out: allow any file, the import tells what it was
if (android) $<HTMLInputElement>("file").removeAttribute("accept");
const pickFile = () => $<HTMLInputElement>("file").click();

async function toggle() {
  if (busy) return;
  if (!index.items.length) return pickFile();
  busy = true;
  orb?.ripple();
  try {
    if (status.state === "off") await api.connect();
    else await api.disconnect();
  } catch (e) {
    toast(errText(e));
  } finally {
    busy = false;
  }
}

// a real <button> sits over the sphere: click, Enter and Space all come for free
power.addEventListener("click", (e) => {
  // Android has no drag & drop: the orb offers every way to import
  if (android && !index.items.length) {
    e.stopPropagation();
    return openMenu(true);
  }
  // the pointer is still resting on the orb: don't let the hover glyph sit on
  // top of what the click just set in motion; it returns on the next visit
  power.classList.add("spent");
  orb?.hover(false);
  void toggle();
});
power.addEventListener("pointerenter", () => orb?.hover(true));
power.addEventListener("pointerleave", () => {
  power.classList.remove("spent");
  orb?.hover(false);
  orb?.press(false);
});
power.addEventListener("pointerdown", () => orb?.press(true));
power.addEventListener("pointerup", () => orb?.press(false));
power.addEventListener("focus", () => power.matches(":focus-visible") && orb?.hover(true));
power.addEventListener("blur", () => {
  power.classList.remove("spent");
  orb?.hover(power.matches(":hover"));
});

// ---------- server picker ----------

const menu = $("menu");
const picker = $<HTMLButtonElement>("picker");

// ---------- country flag: looked up once per server from its IP ----------

// square flags, bundled; only the one shown is ever loaded
const FLAGS = import.meta.glob<string>("../node_modules/flag-icons/flags/1x1/*.svg", { query: "?url", import: "default" });
const flagLoader = (cc: string) => FLAGS[`../node_modules/flag-icons/flags/1x1/${cc}.svg`];
const flagLooks = new Map<string, Promise<{ url: string; ring: string; glow: string } | null>>();
const asked = new Set<string>();

/** The flag's own colours, by area, as a soft ring around it. */
function flagLook(cc: string) {
  let p = flagLooks.get(cc);
  if (p) return p;
  p = (async () => {
    const load = flagLoader(cc);
    if (!load) return null;
    const url = await load();
    const img = new Image();
    img.src = url;
    await img.decode();
    const c = document.createElement("canvas");
    c.width = c.height = 24;
    const g = c.getContext("2d", { willReadFrequently: true })!;
    g.drawImage(img, 0, 0, 24, 24);
    const px = g.getImageData(0, 0, 24, 24).data;
    const counts = new Map<string, { n: number; rgb: number[] }>();
    for (let i = 0; i < px.length; i += 4) {
      const rgb = [px[i], px[i + 1], px[i + 2]];
      const key = rgb.map((v) => v >> 5).join();
      const e = counts.get(key) ?? { n: 0, rgb };
      e.n++;
      counts.set(key, e);
    }
    const top = [...counts.values()].sort((a, b) => b.n - a.n).filter((e) => e.n >= 24 * 24 * 0.06).slice(0, 4);
    const hex = top.map((e) => `rgb(${e.rgb.join(" ")})`);
    if (!hex.length) return null;
    // the most colourful one lights the glow; white rarely is
    const vivid = [...top].sort((a, b) => sat(b.rgb) - sat(a.rgb))[0].rgb;
    // a closed loop of the flag's colours; CSS spins it
    const ring = [...hex, hex[0]].join(", ");
    return { url, ring, glow: `rgb(${vivid.join(" ")} / 0.4)` };
  })().catch(() => null);
  flagLooks.set(cc, p);
  return p;
}
function sat([r, g, b]: number[]) {
  return Math.max(r, g, b) - Math.min(r, g, b);
}

/** Find out where a server is (once), then remember it with the config. */
async function locate(it: Item) {
  if (it.country || asked.has(it.id)) return;
  asked.add(it.id);
  try {
    const ip = await api.serverIp(it.id);
    const r = await fetch(`https://api.country.is/${encodeURIComponent(ip)}`, { signal: AbortSignal.timeout(8000) });
    const cc = String((await r.json()).country ?? "").toLowerCase();
    if (/^[a-z]{2}$/.test(cc) && flagLoader(cc)) {
      index = await api.setCountry(it.id, cc);
      renderPicker();
    }
  } catch {
    // offline or the lookup failed: the monogram stays, we'll try next launch
  }
}

let shownFlag = "";
async function renderFlag(it: Item | null) {
  const badge = $("badge");
  const cc = it?.country ?? "";
  if (cc === shownFlag) return;
  shownFlag = cc;
  const look = cc ? await flagLook(cc) : null;
  if (shownFlag !== cc) return; // a newer render won
  badge.classList.toggle("has-flag", !!look);
  if (look) {
    $("flag").style.backgroundImage = `url("${look.url}")`;
    badge.style.setProperty("--flag-colors", look.ring);
    badge.style.setProperty("--flag-glow", look.glow);
  }
}

function renderPicker() {
  const it = active();
  $("pickName").textContent = it?.name ?? "Import a config";
  $("pickHost").textContent = it?.host ?? (android ? "A file, QR code or the clipboard" : "Choose a .conf file");
  $("mono").textContent = mono(it);
  void renderFlag(it);
  if (it) void locate(it);
  picker.setAttribute("aria-label", it ? `Server: ${it.name}` : "Import a WireGuard config");
}

const CHECK = `<svg class="check" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 8.5l3.2 3.2L13 4.5"/></svg>`;
const PLUS = `<svg class="plus" viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10"/></svg>`;
const QR = `<svg class="plus" viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 6V2.5H6M10 2.5h3.5V6M13.5 10v3.5H10M6 13.5H2.5V10M5.5 8h5"/></svg>`;
const PASTE = `<svg class="plus" viewBox="0 0 16 16" aria-hidden="true"><path d="M6 2.5h4v2H6zM10.5 3.5h1.5v10H4v-10h1.5M6.5 8.5h3M6.5 11h3"/></svg>`;
const PEN = `<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M6.6 1.9l1.5 1.5L3.4 8.1 1.5 8.5l.4-1.9z"/></svg>`;
const CROSS = `<svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2 2l6 6M8 2l-6 6"/></svg>`;
const STARS = `<span class="stars" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>`;
const esc = (s: string) => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

function renderMenu() {
  const sel = index.selected;
  const at = index.items.findIndex((x) => x.id === sel);
  const top = menu.querySelector(".rows")?.scrollTop ?? -1;
  menu.innerHTML =
    `<div class="rows">` +
    index.items
      .map((it, i) => {
        const on = it.id === sel;
        // rows sink into the dark the further they are from the selected one
        const fade = on ? 1 : Math.max(0.3, 0.66 - (Math.abs(i - at) - 1) * 0.13);
        return `<div class="row${on ? " on" : ""}" role="option" tabindex="0" aria-selected="${on}" data-id="${esc(it.id)}" style="--fade:${fade.toFixed(2)}">
          ${on ? STARS + CHECK : `<span class="check"></span>`}
          <span class="row-text"><span class="row-name">${esc(it.name)}</span><span class="row-host">${esc(it.host)}</span></span>
          <button class="rm ren" type="button" data-ren="${esc(it.id)}" aria-label="Rename ${esc(it.name)}">${PEN}</button>
          <button class="rm" type="button" data-rm="${esc(it.id)}" aria-label="Remove ${esc(it.name)}">${CROSS}</button>
        </div>`;
      })
      .join("") +
    `</div><div class="row add" role="option" tabindex="0" aria-selected="false" data-add="1" style="--fade:.62">${PLUS}<span class="row-text"><span class="row-name">Import config…</span></span></div>` +
    (android
      ? `<div class="row add" role="option" tabindex="0" aria-selected="false" data-scan="1" style="--fade:.62">${QR}<span class="row-text"><span class="row-name">Scan QR code</span></span></div>` +
        `<div class="row add" role="option" tabindex="0" aria-selected="false" data-paste="1" style="--fade:.62">${PASTE}<span class="row-text"><span class="row-name">Paste config</span></span></div>`
      : "");
  // a long list scrolls under a soft fade; keep the place, or start at the selected row
  const rows = menu.querySelector<HTMLElement>(".rows")!;
  rows.classList.toggle("scrolls", rows.scrollHeight > rows.clientHeight + 1);
  if (top >= 0) rows.scrollTop = top;
  else rows.querySelector<HTMLElement>(".on")?.scrollIntoView({ block: "nearest" });
}

let menuTimer = 0;
const menuOpen = () => document.body.classList.contains("menu-open");

function openMenu(open: boolean, focus = false) {
  if (open === menuOpen()) return;
  if (open) {
    openSettings(false);
    openSheet(false);
    renderMenu();
  }
  document.body.classList.toggle("menu-open", open);
  picker.setAttribute("aria-expanded", String(open));
  if (open && focus) menu.querySelector<HTMLElement>(".row.on, .row")?.focus();
  if (!open && menu.contains(document.activeElement)) picker.focus();
  // once it has faded out, drop the rows so their twinkling stars stop animating
  clearTimeout(menuTimer);
  if (!open) menuTimer = window.setTimeout(() => (menu.textContent = ""), 400);
}

picker.addEventListener("click", (e) => {
  e.stopPropagation();
  // Android has no drag & drop or paste shortcut: the menu offers both imports
  if (!index.items.length && !android) return pickFile();
  // e.detail is 0 when the click came from the keyboard
  openMenu(!menuOpen(), e.detail === 0);
});
document.addEventListener("click", (e) => {
  if (!menu.contains(e.target as Node)) openMenu(false);
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") openMenu(false);
});

async function chooseRow(row: HTMLElement) {
  if (row.dataset.add) {
    openMenu(false);
    return pickFile();
  }
  if (row.dataset.paste) {
    openMenu(false);
    return pasteConfig();
  }
  if (row.dataset.scan) {
    openMenu(false);
    return scanConfig();
  }
  const id = row.dataset.id!;
  if (id !== index.selected) {
    const wasOn = status.state !== "off";
    index = await api.select(id);
    renderMenu();
    // switching servers while connected reconnects to the new one
    if (wasOn) {
      await api.disconnect();
      await api.connect().catch((err) => toast(errText(err)));
    }
    renderStatus();
  }
  setTimeout(() => openMenu(false), 180);
}

menu.addEventListener("click", async (e) => {
  e.stopPropagation();
  const t = e.target as HTMLElement;
  const ren = t.closest<HTMLElement>("[data-ren]");
  if (ren) {
    openMenu(false);
    return openRename(ren.dataset.ren!);
  }
  const rm = t.closest<HTMLElement>("[data-rm]");
  if (rm) {
    try {
      index = await api.remove(rm.dataset.rm!);
      renderMenu();
      renderStatus();
      if (!index.items.length) openMenu(false);
    } catch (err) {
      toast(errText(err));
    }
    return;
  }
  const row = t.closest<HTMLElement>(".row");
  if (row) await chooseRow(row);
});

menu.addEventListener("keydown", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".row");
  if (!row || (e.target as HTMLElement).closest(".rm")) return;
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    void chooseRow(row);
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const rows = [...menu.querySelectorAll<HTMLElement>(".row")];
    rows[(rows.indexOf(row) + (e.key === "ArrowDown" ? 1 : rows.length - 1)) % rows.length]?.focus();
  }
});

// ---------- import: file picker, drag & drop, paste ----------

async function importTexts(files: { text: string; name: string | null }[]) {
  let ok = 0;
  for (const f of files) {
    try {
      await api.importConfig(f.text, f.name);
      details = null; // a re-import can replace the config behind an id
      ok++;
    } catch (e) {
      toast(f.name ? `${f.name}: ${errText(e)}` : errText(e));
    }
  }
  if (ok) {
    index = await api.list();
    renderStatus();
    if (menuOpen()) renderMenu();
  }
}

/** A picture is taken to be a QR code of a config. */
async function readQr(f: File) {
  try {
    return { text: await api.decodeQr(new Uint8Array(await f.arrayBuffer())), name: null };
  } catch (e) {
    toast(`${f.name}: ${errText(e)}`);
    return null;
  }
}

async function importFiles(list: FileList | File[]) {
  const isImage = (f: File) => f.type.startsWith("image/");
  const images = [...list].filter(isImage);
  const all = [...list].filter((f) => !isImage(f));
  // a WireGuard config is a few hundred bytes; anything big is something else
  for (const f of all.filter((f) => f.size >= 64_000)) toast(`${f.name}: This doesn't look like a WireGuard config.`);
  const files = await Promise.all(
    all.filter((f) => f.size < 64_000).map(async (f) => ({ text: await f.text(), name: f.name })),
  );
  const codes = (await Promise.all(images.filter((f) => f.size < 30_000_000).map(readQr))).filter((c) => c !== null);
  await importTexts([...files, ...codes]);
}

$<HTMLInputElement>("file").addEventListener("change", async (e) => {
  const input = e.target as HTMLInputElement;
  if (input.files) await importFiles(input.files);
  input.value = "";
});

let dragDepth = 0;
window.addEventListener("dragenter", (e) => {
  e.preventDefault();
  dragDepth++;
  document.body.classList.add("dragging");
});
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("dragleave", () => {
  if (--dragDepth <= 0) {
    dragDepth = 0;
    document.body.classList.remove("dragging");
  }
});
window.addEventListener("drop", async (e) => {
  e.preventDefault();
  dragDepth = 0;
  document.body.classList.remove("dragging");
  if (e.dataTransfer?.files.length) await importFiles(e.dataTransfer.files);
});
// Android: a .conf opened or shared with Prism from another app
async function importOpened() {
  try {
    const o = await api.takeOpened();
    if (o) await importTexts([{ text: o[0], name: o[1] }]);
  } catch (e) {
    toast(errText(e));
  }
}
async function scanConfig() {
  try {
    const text = await api.scanQr();
    if (text === null) return;
    if (/\[interface\]/i.test(text)) await importTexts([{ text, name: null }]);
    else toast("This QR code isn't a WireGuard config.");
  } catch (e) {
    toast(errText(e));
  }
}
async function pasteConfig() {
  const text = await api.pasteText().catch(() => "");
  if (/\[interface\]/i.test(text)) await importTexts([{ text, name: null }]);
  else toast("The clipboard has no WireGuard config.");
}
// Android: the Quick Settings tile opens Prism to connect when it can't do it in place
async function connectIfAsked() {
  if (!(await api.takeConnect().catch(() => false))) return;
  if (!index.items.length) return openMenu(true);
  if (status.state === "off") void toggle();
}
if (android) {
  void importOpened().then(connectIfAsked);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") void importOpened().then(connectIfAsked);
  });
}

document.addEventListener("paste", async (e) => {
  const text = e.clipboardData?.getData("text") ?? "";
  if (/\[interface\]/i.test(text)) await importTexts([{ text, name: null }]);
});

// ---------- toast ----------

let toastTimer = 0;
function toast(msg: string, kind: "error" | "info" = "error") {
  const el = $("toast");
  el.textContent = msg;
  // "note", not "info": .info is the details button, whose styles it would take
  el.classList.toggle("note", kind === "info");
  el.classList.add("show");
  clearTimeout(toastTimer);
  // a confirmation is read at a glance; an error needs time
  toastTimer = window.setTimeout(() => el.classList.remove("show"), kind === "info" ? 1500 : 5200);
}

// ---------- details: what the WireGuard app shows ----------

const sheet = $("sheet");
const infoBtn = $<HTMLButtonElement>("info");
let lastStats: Stats | null = null;
let details: { id: string; d: Details } | null = null;
const sheetOpen = () => document.body.classList.contains("sheet-open");

function bytes(n: number) {
  if (n < 1000) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let v = n / 1000, i = 0;
  while (v >= 1000 && i < u.length - 1) (v /= 1000), i++;
  return `${v.toFixed(v < 10 ? 2 : v < 100 ? 1 : 0)} ${u[i]}`;
}
function ago(sec: number | null) {
  if (sec == null) return "Not yet";
  if (sec < 60) return `${sec} s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)} min ${sec % 60} s ago`;
  return `${Math.floor(sec / 3600)} h ago`;
}
const STATE_LABEL: Record<State, string> = {
  off: "Inactive",
  connecting: "Connecting",
  connected: "Active",
  unreachable: "No answer",
  stalled: "Reconnecting",
};

function el<K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
function section(title: string, rows: [string, string, { mono?: boolean; copy?: boolean; id?: string }?][]) {
  const s = el("section");
  s.append(el("h3", undefined, title));
  for (const [label, value, o = {}] of rows) {
    const r = el(o.copy ? "button" : "div", "kv");
    if (o.copy) {
      (r as HTMLButtonElement).type = "button";
      r.dataset.copy = value;
      r.title = "Copy";
    }
    const v = el("span", "v" + (o.mono ? " mono" : ""), value);
    if (o.id) v.id = o.id;
    r.append(el("span", "k", label), v);
    s.append(r);
  }
  return s;
}

function renderSheet() {
  sheet.textContent = "";
  const it = index.items.find((i) => i.id === index.selected);
  if (!it || !details || details.id !== it.id) return;
  const d = details.d;
  const live = status.state !== "off" && status.configId === it.id;

  const head = el("div", "sheet-head");
  head.append(el("span", "sheet-title", it.name));
  const pill = el("span", "pill", STATE_LABEL[live ? status.state : "off"]);
  pill.dataset.state = live ? status.state : "off";
  head.append(pill);

  const body = el("div", "sheet-body");
  if (live)
    body.append(
      section("Tunnel", [
        ["Uptime", status.since ? elapsed(status.since) : "—", { mono: true, id: "dUp" }],
        ["Latest handshake", ago(lastStats?.handshakeAge ?? null), { id: "dHs" }],
        ["Routing", index.smart ? "Russia direct" : "Everything via VPN"],
        ["Received", bytes(lastStats?.rx ?? 0), { mono: true, id: "dRx" }],
        ["Sent", bytes(lastStats?.tx ?? 0), { mono: true, id: "dTx" }],
      ]),
    );
  body.append(
    section("Interface", [
      ["Public key", d.publicKey, { mono: true, copy: true }],
      ["Addresses", d.addresses || "—", { mono: true }],
      ["DNS servers", d.dns || "—", { mono: true }],
      ["MTU", String(d.mtu), { mono: true }],
    ]),
    section("Peer", [
      ["Public key", d.peerKey, { mono: true, copy: true }],
      ...(d.preshared ? ([["Preshared key", "Enabled"]] as [string, string][]) : []),
      ["Endpoint", d.endpoint, { mono: true, copy: true }],
      ["Allowed IPs", d.allowedIps, { mono: true }],
      ["Keepalive", d.keepalive ? `Every ${d.keepalive} s` : "Off"],
    ]),
  );
  sheet.append(head, body);
  liveSheet();
}

/** Ticks the live rows without rebuilding the sheet. */
function liveSheet() {
  if (!sheetOpen() || document.hidden) return;
  const set = (id: string, v: string) => {
    const e = document.getElementById(id);
    if (e && e.textContent !== v) e.textContent = v;
  };
  if (status.since) set("dUp", elapsed(status.since));
  if (lastStats) {
    set("dHs", ago(lastStats.handshakeAge));
    set("dRx", bytes(lastStats.rx));
    set("dTx", bytes(lastStats.tx));
  }
}
setInterval(liveSheet, 1000);

function fitSheet() {
  // grow up to the title bar, scroll inside past that
  const top = $("picker").closest(".dock")!.getBoundingClientRect().top;
  if (android) {
    // viewport px → the page's zoomed px, below the title bar
    const k = Number(getComputedStyle(document.documentElement).getPropertyValue("--k")) || 1;
    const bar = document.querySelector(".titlebar")!.getBoundingClientRect().bottom;
    sheet.style.maxHeight = `${Math.max(160, (top - bar) / k - 10)}px`;
    return;
  }
  sheet.style.maxHeight = `${Math.max(160, top - 48 - 10)}px`;
}

async function openSheet(open: boolean) {
  if (open === sheetOpen()) return;
  if (open) {
    const id = index.selected;
    if (!id) return;
    openMenu(false);
    openSettings(false);
    try {
      if (details?.id !== id) details = { id, d: await api.details(id) };
    } catch (err) {
      return toast(errText(err));
    }
    renderSheet();
    fitSheet();
  }
  document.body.classList.toggle("sheet-open", open);
  infoBtn.setAttribute("aria-expanded", String(open));
  if (!open && sheet.contains(document.activeElement)) infoBtn.focus();
}

infoBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  void openSheet(!sheetOpen());
});
sheet.addEventListener("click", async (e) => {
  e.stopPropagation();
  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-copy]");
  if (!row) return;
  try {
    await navigator.clipboard.writeText(row.dataset.copy!);
    toast("Copied", "info");
  } catch {
    toast("Couldn't copy");
  }
});
document.addEventListener("click", () => void openSheet(false));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") void openSheet(false);
});

// ---------- window buttons (Windows; macOS has native ones) ----------

$("min").addEventListener("click", () => api.minimize());
$("close").addEventListener("click", () => api.close());

// ---------- settings ----------

const settings = $("settings");
const gear = $<HTMLButtonElement>("gear");
const seg = $("themeSeg");
const settingsOpen = () => document.body.classList.contains("settings-open");

const smartBtn = $<HTMLButtonElement>("smart");
function renderSettings() {
  smartBtn.setAttribute("aria-checked", String(index.smart));
  const order = ["system", "light", "dark"];
  seg.style.setProperty("--at", String(Math.max(order.indexOf(index.theme), 0)));
  for (const b of seg.querySelectorAll<HTMLButtonElement>("button")) {
    const on = b.dataset.theme === index.theme;
    b.setAttribute("aria-checked", String(on));
    b.tabIndex = on ? 0 : -1;
  }
}

function openSettings(open: boolean) {
  if (open === settingsOpen()) return;
  if (open) {
    openMenu(false);
    openSheet(false);
    renderSettings();
  }
  document.body.classList.toggle("settings-open", open);
  gear.setAttribute("aria-expanded", String(open));
  if (open) seg.querySelector<HTMLElement>('[aria-checked="true"]')?.focus({ preventScroll: true });
  else if (settings.contains(document.activeElement)) gear.focus();
}

gear.addEventListener("click", (e) => {
  e.stopPropagation();
  openSettings(!settingsOpen());
});
settings.addEventListener("click", (e) => e.stopPropagation());
document.addEventListener("click", () => openSettings(false));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") openSettings(false);
});

async function chooseTheme(theme: Index["theme"]) {
  if (theme === index.theme) return;
  index = { ...index, theme };
  renderSettings();
  applyTheme(false);
  try {
    index = await api.setTheme(theme);
  } catch (err) {
    toast(errText(err));
  }
}
seg.addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLButtonElement>("button");
  if (b) void chooseTheme(b.dataset.theme as Index["theme"]);
});
// arrow keys move along the segmented control, like a native radio group
seg.addEventListener("keydown", (e) => {
  const order: Index["theme"][] = ["system", "light", "dark"];
  const d = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
  if (!d) return;
  e.preventDefault();
  const next = order[(order.indexOf(index.theme) + d + 3) % 3];
  void chooseTheme(next).then(() => seg.querySelector<HTMLElement>(`[data-theme="${next}"]`)?.focus());
});
smartBtn.addEventListener("click", async () => {
  const on = !index.smart;
  index = { ...index, smart: on };
  renderSettings();
  try {
    // a live tunnel reconnects with the new routing
    index = await api.setSmart(on);
  } catch (err) {
    toast(errText(err));
  }
  renderSettings();
});
$("quit").addEventListener("click", () => api.quit());
$("gh").addEventListener("click", () => api.openGithub());
void api.version().then((v) => ($("ver").textContent = v));

// ---------- rename ----------

const renameBox = $("rename");
const renameInput = $<HTMLInputElement>("renameInput");
let renaming: string | null = null;
const renameOpen = () => document.body.classList.contains("rename-open");

function openRename(id: string | null) {
  renaming = id;
  const it = index.items.find((i) => i.id === id);
  if (id && it) {
    openSettings(false);
    openSheet(false);
    renameInput.value = it.name;
    document.body.classList.add("rename-open");
    renameInput.focus();
    renameInput.select();
  } else {
    document.body.classList.remove("rename-open");
    renameInput.blur();
  }
}

async function saveRename() {
  const id = renaming;
  const name = renameInput.value.trim();
  if (!id || !name) return openRename(null);
  try {
    index = await api.rename(id, name);
    renderStatus();
    openRename(null);
  } catch (e) {
    toast(errText(e));
  }
}

renameBox.addEventListener("click", (e) => e.stopPropagation());
document.addEventListener("click", () => renameOpen() && openRename(null));
$("renameCancel").addEventListener("click", () => openRename(null));
$("renameSave").addEventListener("click", () => void saveRename());
renameInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") void saveRename();
  if (e.key === "Escape") openRename(null);
});

// Android back gesture: close whatever is open, topmost first (VpnPlugin.kt
// asks; false lets Prism go to the background)
(window as unknown as { __prismBack: () => boolean }).__prismBack = () => {
  if (renameOpen()) openRename(null);
  else if (sheetOpen()) void openSheet(false);
  else if (settingsOpen()) openSettings(false);
  else if (menuOpen()) openMenu(false);
  else return false;
  return true;
};

// ---------- wire up ----------

api.onStatus((s) => {
  const wasLive = status.state !== "off";
  status = s;
  renderStatus();
  if (s.state === "off") lastStats = null;
  // the Tunnel section comes and goes with the connection
  if (sheetOpen() && wasLive !== (s.state !== "off")) renderSheet();
  else if (sheetOpen()) {
    const pill = sheet.querySelector<HTMLElement>(".pill");
    if (pill && s.state !== "off") {
      pill.textContent = STATE_LABEL[s.state];
      pill.dataset.state = s.state;
    }
  }
});
api.onStats(onStats);
renderStatus();
if (android) {
  // A cold WebView stutters the first time it draws each kind of thing (it
  // compiles GPU shaders on demand): the glass panels, their shadows, the
  // shrinking orb. Play those animations once behind an opaque cover the
  // colour of the launch screen, then lift the cover.
  const cover = document.createElement("div");
  cover.style.cssText =
    `position:fixed;inset:0;z-index:2147483647;transition:opacity .32s ease;` +
    `background:${getComputedStyle(document.documentElement).getPropertyValue("--bg").trim() || "#050507"}`;
  document.documentElement.append(cover);
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  const panels = ["menu-open", "settings-open", "sheet-open"];
  // the page must be drawn (not transparent) and animating for this to count
  document.body.classList.add("ready");
  void (async () => {
    await Promise.race([document.fonts.ready, wait(400)]);
    await wait(50);
    if (index.items.length) renderMenu();
    // the panels, and what a connect sets moving (stats tray, title swap)
    document.body.classList.add(...panels);
    document.body.dataset.state = "connected";
    $("title").parentElement!.classList.add("swap");
    await wait(320);
    document.body.classList.remove(...panels);
    renderStatus();
    await wait(260);
    menu.textContent = "";
    cover.style.pointerEvents = "none";
    cover.style.opacity = "0";
    setTimeout(() => cover.remove(), 400);
  })();
} else {
  requestAnimationFrame(() => document.body.classList.add("ready"));
}

// `instant`: no transitions, so headless screenshots show final states
if (dev?.has("instant")) {
  const st = document.createElement("style");
  st.textContent = "*,*::before,*::after{transition:none!important}";
  document.head.append(st);
}
// (on phones, after the launch warm-up has closed its panels again)
setTimeout(() => {
  if (dev?.has("menu")) openMenu(true);
  if (dev?.has("settings")) openSettings(true);
  if (dev?.has("rename")) openRename(index.selected);
  if (dev?.has("sheet")) {
    if (status.state === "connected") onStats({ rx: 734_220_000, tx: 41_800_000, handshakeAge: 37, level: 0 });
    void openSheet(true);
  }
}, android ? 1200 : 0);
if (dev?.has("toast"))
  setTimeout(
    () => toast(dev.get("toast") || "server.conf: The [Peer] section has no Endpoint.", dev.has("info") ? "info" : "error"),
    android ? 1300 : 0,
  );
if (dev?.has("drag")) document.body.classList.add("dragging");
// `sim=connecting:1.2,ripple,connected:0.4` plays orb states forward and freezes on that frame
for (const part of dev?.get("sim")?.split(",") ?? []) {
  const [st, sec] = part.split(":");
  if (st === "ripple") orb?.ripple();
  else orb?.set(ORB[st as State]);
  orb?.advance(Number(sec ?? 0));
}
if (dev?.has("hover")) {
  power.classList.add("hover");
  orb?.hover(true);
}
