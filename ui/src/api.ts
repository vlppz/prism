// Thin layer over Tauri. Outside Tauri (plain browser) a small mock runs,
// so the UI can be developed and screenshotted: add `#demo=connected` etc.

export type State = "off" | "connecting" | "connected" | "unreachable" | "stalled";

export interface Item {
  id: string;
  name: string;
  host: string;
  custom: boolean;
  /** ISO country code of the server, once known. */
  country?: string | null;
}
export type Theme = "system" | "light" | "dark";
export interface Index {
  items: Item[];
  selected: string | null;
  theme: Theme;
  smart: boolean;
}
export interface Status {
  state: State;
  configId: string | null;
  error: string | null;
  since: number | null;
}
export interface Details {
  publicKey: string;
  addresses: string;
  dns: string;
  mtu: number;
  peerKey: string;
  preshared: boolean;
  endpoint: string;
  allowedIps: string;
  keepalive: number | null;
}
export interface Stats {
  rx: number;
  tx: number;
  handshakeAge: number | null;
  level: number;
}

export interface Api {
  list(): Promise<Index>;
  importConfig(text: string, fileName: string | null): Promise<Item>;
  remove(id: string): Promise<Index>;
  select(id: string): Promise<Index>;
  status(): Promise<Status>;
  connect(): Promise<void>;
  disconnect(): Promise<void>;
  platform(): Promise<"macos" | "windows" | "android" | "other">;
  /** Android: clipboard text, read natively (no page prompt). */
  pasteText(): Promise<string>;
  /** Android: a config opened with Prism from another app, once. */
  takeOpened(): Promise<[string, string | null] | null>;
  /** The text of the QR code in a picture. */
  decodeQr(image: Uint8Array): Promise<string>;
  /** Android: camera QR scan; null if the user backed out. */
  scanQr(): Promise<string | null>;
  /** Android: the Quick Settings tile opened Prism to connect. */
  takeConnect(): Promise<boolean>;
  /** Android: offers the Quick Settings tile. */
  addTile(): Promise<"added" | "already" | "declined" | "manual">;
  setTheme(theme: Theme): Promise<Index>;
  setSmart(on: boolean): Promise<Index>;
  details(id: string): Promise<Details>;
  serverIp(id: string): Promise<string>;
  rename(id: string, name: string): Promise<Index>;
  setCountry(id: string, country: string): Promise<Index>;
  version(): Promise<string>;
  quit(): void;
  openGithub(): void;
  onStatus(f: (s: Status) => void): void;
  onStats(f: (s: Stats) => void): void;
  minimize(): void;
  close(): void;
}

const inTauri = "__TAURI_INTERNALS__" in window;

async function tauriApi(): Promise<Api> {
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  const { getVersion } = await import("@tauri-apps/api/app");
  return {
    list: () => invoke("list"),
    importConfig: (text, fileName) => invoke("import_config", { text, fileName }),
    remove: (id) => invoke("remove", { id }),
    select: (id) => invoke("select", { id }),
    status: () => invoke("status"),
    connect: () => invoke("connect"),
    disconnect: () => invoke("disconnect"),
    platform: () => invoke("platform"),
    pasteText: () => invoke("paste_text"),
    takeOpened: () => invoke("take_opened"),
    decodeQr: (image) => invoke("decode_qr", image),
    scanQr: () => invoke("scan_qr"),
    takeConnect: () => invoke("take_connect"),
    addTile: () => invoke("add_tile"),
    setTheme: (theme) => invoke("set_theme", { theme }),
    setSmart: (on) => invoke("set_smart", { on }),
    details: (id) => invoke("details", { id }),
    serverIp: (id) => invoke("server_ip", { id }),
    setCountry: (id, country) => invoke("set_country", { id, country }),
    rename: (id, name) => invoke("rename", { id, name }),
    version: () => getVersion(),
    quit: () => void invoke("quit_app"),
    openGithub: () => void invoke("open_github"),
    onStatus: (f) => void listen<Status>("status", (e) => f(e.payload)),
    onStats: (f) => void listen<Stats>("stats", (e) => f(e.payload)),
    minimize: () => void getCurrentWindow().minimize(),
    close: () => void getCurrentWindow().close(),
  };
}

function mockApi(): Api {
  const demo = new URLSearchParams(location.hash.slice(1));
  let index: Index = {
    items: [
      { id: "a", name: "frankfurt", host: "fra.vpn.example.com", custom: false, country: "de" },
      { id: "b", name: "helsinki", host: "hel.vpn.example.com", custom: false, country: "fi" },
      { id: "c", name: "stockholm", host: "sto.vpn.example.com", custom: false, country: "se" },
      { id: "d", name: "warsaw", host: "waw.vpn.example.com", custom: false, country: "pl" },
    ],
    selected: "a",
    theme: (demo.get("theme") as Theme) ?? "system",
    smart: demo.get("smart") !== "0",
  };
  if (demo.get("many"))
    index.items.push(
      ...["amsterdam", "zurich", "new-york", "tokyo", "home-router"].map((name, i) => ({
        id: `m${i}`,
        name,
        host: i === 4 ? "vpn.a-rather-long-hostname-for-testing.example.org:51820" : `${name}.vpn.example.com`,
        custom: i === 4,
      })),
    );
  if (demo.get("empty")) index = { ...index, items: [], selected: null };
  if (demo.has("cc")) index.items[0].country = demo.get("cc") || null;
  const state = (demo.get("demo") as State) ?? "off";
  let status: Status = { state, configId: state === "off" ? null : index.selected, error: demo.get("error"), since: Date.now() - 754_000 };
  let statusCb: (s: Status) => void = () => {};
  let statsCb: (s: Stats) => void = () => {};
  let rx = 48_000_000, tx = 3_100_000;
  setInterval(() => {
    if (status.state === "connected") {
      rx += 1_800_000 + Math.random() * 900_000;
      tx += 120_000 + Math.random() * 80_000;
      statsCb({ rx, tx, handshakeAge: 12, level: 0 });
    }
  }, 1000);
  const set = (s: Partial<Status>) => {
    status = { ...status, ...s };
    statusCb(status);
  };
  return {
    list: async () => index,
    importConfig: async (_t, f) => {
      const it = { id: String(Math.random()), name: (f ?? "server").replace(/\.conf$/, ""), host: "example.com", custom: false };
      index = { ...index, items: [...index.items, it], selected: it.id };
      return it;
    },
    remove: async (id) => (index = { ...index, items: index.items.filter((i) => i.id !== id), selected: index.items.find((i) => i.id !== id)?.id ?? null }),
    select: async (id) => (index = { ...index, selected: id }),
    status: async () => status,
    connect: async () => {
      set({ state: "connecting", configId: index.selected, error: null, since: null });
      setTimeout(() => set({ state: "connected", since: Date.now() }), 2200);
    },
    disconnect: async () => set({ state: "off", configId: null, since: null }),
    platform: async () => (demo.get("win") ? "windows" : demo.has("android") ? "android" : "macos"),
    pasteText: async () => "",
    takeOpened: async () => null,
    decodeQr: async () => {
      throw "no QR code found in this picture";
    },
    scanQr: async () => null,
    takeConnect: async () => false,
    addTile: async () => "manual",
    setTheme: async (theme) => (index = { ...index, theme }),
    setSmart: async (smart) => (index = { ...index, smart }),
    serverIp: async () => "192.0.2.1",
    setCountry: async (id, country) =>
      (index = { ...index, items: index.items.map((i) => (i.id === id ? { ...i, country } : i)) }),
    rename: async (id, name) => (index = { ...index, items: index.items.map((i) => (i.id === id ? { ...i, name } : i)) }),
    details: async (id) => ({
      publicKey: "kQ3v8yJ0m2cRZb7xPq1Lw9sTn4uVfE6hGdA5oKiMcXY=",
      addresses: "10.14.0.2/16",
      dns: "162.252.172.57, 149.154.159.92",
      mtu: 1420,
      peerKey: "o07k/2dsaQkLLSR0dCI/FUd3FLik/F/HBBcOGUkNQGo=",
      preshared: false,
      endpoint: `${index.items.find((i) => i.id === id)?.host ?? "example.com"}:51820`,
      allowedIps: "0.0.0.0/0",
      keepalive: 25,
    }),
    version: async () => "0.1.0",
    quit: () => {},
    openGithub: () => void window.open("https://github.com/vlppz"),
    onStatus: (f) => (statusCb = f),
    onStats: (f) => (statsCb = f),
    minimize: () => {},
    close: () => {},
  };
}

export const api: Promise<Api> = inTauri ? tauriApi() : Promise.resolve(mockApi());
