import { create } from "zustand";
import {
  api,
  ImmichSettings,
  ImmichStatus,
  defaultSettings,
  DestIndexEvent,
  ImportProgress,
  ItemView,
  Outcome,
  Settings,
  Summary,
  Volume,
} from "./api";

export type Filter = "all" | "new" | "duplicates";
export type ItemOutcome = "imported" | "already_present" | "skipped" | "failed";

interface State {
  volumes: Volume[];
  source: string | null;
  sourceRoot: string | null;
  scanning: boolean;
  scanError: string | null;
  gen: number;
  items: ItemView[];
  selected: Set<number>;
  /** Items the user toggled by hand; auto-selection leaves them alone. */
  touched: Set<number>;
  anchor: number | null;
  filter: Filter;
  settings: Settings;
  settingsLoaded: boolean;
  destIndex: DestIndexEvent | null;
  importing: boolean;
  progress: ImportProgress | null;
  outcomes: Map<number, { outcome: ItemOutcome; error?: string }>;
  summary: Summary | null;
  importError: string | null;
  immich: ImmichSettings | null;
  immichStatus: ImmichStatus;
  settingsOpen: boolean;
  /** Where the last import went, and how many files landed in each folder. */
  importRoot: string | null;
  importedDirs: Record<string, number>;
  /** The files the last import copied, in the order it copied them. */
  importedFiles: string[];
  /** Hooks an app embedding the importer may set (embed.tsx). */
  openSettings: () => void;
  showImported: (() => void) | null;

  setVolumes: (v: Volume[]) => void;
  openSource: (path: string) => Promise<void>;
  applyUpdates: (gen: number, items: ItemView[]) => void;
  toggle: (id: number, shift: boolean, visibleIds: number[]) => void;
  selectAll: (ids: number[]) => void;
  selectNone: () => void;
  setFilter: (f: Filter) => void;
  updateSettings: (patch: Partial<Settings>) => void;
  loadSettings: () => Promise<void>;
  setDestIndex: (e: DestIndexEvent) => void;
  startImport: (ids?: number[]) => Promise<void>;
  setProgress: (p: ImportProgress) => void;
  recordOutcome: (itemId: number, outcome: Outcome) => void;
  finishImport: (s: Summary) => void;
  dismissSummary: () => void;
  loadImmich: () => Promise<void>;
  setImmichStatus: (s: ImmichStatus) => void;
  setSettingsOpen: (open: boolean) => void;
  /** The UI couldn't decode a frame of this video (shows a film icon). */
  markThumbFailed: (id: number) => void;
}

/** The settings that decide which duplicates may be imported. */
export type ImportPolicy = Pick<Settings, "importDuplicates" | "importDeleted">;

/** May this status be imported at all under the policy? (in_destination never.) */
function allowed(status: ItemView["status"], p: ImportPolicy): boolean {
  switch (status) {
    case "in_destination":
      return false;
    case "duplicate":
    case "in_immich":
      return p.importDuplicates;
    case "deleted":
      return p.importDeleted;
    default:
      return true;
  }
}

/** Selected by default: new photos, plus duplicates the policy allows. */
export function eligible(item: ItemView, p: ImportPolicy): boolean {
  // "checking"/"checking_immich" wait for the result.
  if (item.status === "checking" || item.status === "checking_immich") return false;
  return allowed(item.status, p);
}

/** Can the user select it at all? */
export function selectable(item: ItemView, p: ImportPolicy): boolean {
  return allowed(item.status, p);
}

function autoSelect(items: ItemView[], selected: Set<number>, touched: Set<number>, p: ImportPolicy) {
  const next = new Set(selected);
  for (const it of items) {
    if (!touched.has(it.id)) {
      if (eligible(it, p)) next.add(it.id);
      else next.delete(it.id);
    } else if (!selectable(it, p)) {
      next.delete(it.id);
    }
  }
  return next;
}

let saveTimer: number | undefined;

export const useStore = create<State>((set, get) => ({
  volumes: [],
  source: null,
  sourceRoot: null,
  scanning: false,
  scanError: null,
  gen: 0,
  items: [],
  selected: new Set(),
  touched: new Set(),
  anchor: null,
  filter: "all",
  settings: defaultSettings,
  settingsLoaded: false,
  destIndex: null,
  importing: false,
  progress: null,
  outcomes: new Map(),
  summary: null,
  importError: null,
  immich: null,
  immichStatus: { state: "off" },
  settingsOpen: false,
  importRoot: null,
  importedDirs: {},
  importedFiles: [],
  openSettings: () => set({ settingsOpen: true }),
  showImported: null,

  setVolumes: (volumes) => set({ volumes }),

  openSource: async (path) => {
    set({
      source: path,
      // A new source starts on "All", so nothing new is hidden by an old filter.
      filter: "all",
      scanning: true,
      scanError: null,
      items: [],
      selected: new Set(),
      touched: new Set(),
      anchor: null,
      outcomes: new Map(),
      summary: null,
    });
    try {
      const { includeSubfolders, skipRejected, includeVideos } = get().settings;
      const r = await api.scanSource(path, includeSubfolders, skipRejected, includeVideos);
      if (get().source !== path) return;
      const { settings } = get();
      set({
        gen: r.gen,
        sourceRoot: r.root,
        items: r.items,
        scanning: false,
        selected: autoSelect(r.items, new Set(), new Set(), settings),
      });
    } catch (e) {
      if (get().source === path) set({ scanning: false, scanError: String(e) });
    }
  },

  applyUpdates: (gen, updates) => {
    const s = get();
    if (gen !== s.gen || s.items.length === 0) return;
    const items = s.items.slice();
    for (const u of updates) {
      // A video frame the UI failed to decode stays failed across updates.
      const keepFailed = items[u.id]?.thumbFailed && u.kind === "video" && !u.thumb;
      items[u.id] = keepFailed ? { ...u, thumbFailed: true } : u;
    }
    set({ items, selected: autoSelect(updates, s.selected, s.touched, s.settings) });
  },

  toggle: (id, shift, visibleIds) => {
    const s = get();
    const dup = s.settings;
    const selected = new Set(s.selected);
    const touched = new Set(s.touched);
    const on = !selected.has(id);
    let ids = [id];
    if (shift && s.anchor !== null) {
      const a = visibleIds.indexOf(s.anchor);
      const b = visibleIds.indexOf(id);
      if (a >= 0 && b >= 0) ids = visibleIds.slice(Math.min(a, b), Math.max(a, b) + 1);
    }
    for (const i of ids) {
      const it = s.items[i];
      if (!it || !selectable(it, dup)) continue;
      touched.add(i);
      if (on) selected.add(i);
      else selected.delete(i);
    }
    set({ selected, touched, anchor: id });
  },

  selectAll: (ids) => {
    const s = get();
    const selected = new Set(s.selected);
    const touched = new Set(s.touched);
    for (const i of ids) {
      if (selectable(s.items[i], s.settings)) {
        selected.add(i);
        touched.add(i);
      }
    }
    set({ selected, touched });
  },

  selectNone: () => {
    const s = get();
    const touched = new Set(s.touched);
    s.items.forEach((it) => touched.add(it.id));
    set({ selected: new Set(), touched });
  },

  setFilter: (filter) => set({ filter }),

  updateSettings: (patch) => {
    const s = get();
    const settings = { ...s.settings, ...patch };
    const changes: Partial<State> = { settings };
    const policyChanged =
      (patch.importDuplicates !== undefined && patch.importDuplicates !== s.settings.importDuplicates) ||
      (patch.importDeleted !== undefined && patch.importDeleted !== s.settings.importDeleted);
    if (policyChanged) {
      // Re-apply defaults to everything when the duplicate policy changes.
      changes.touched = new Set();
      changes.selected = autoSelect(s.items, new Set(), new Set(), settings);
    }
    set(changes);
    // The scan scope changed: rescan the current source.
    const scopeChanged =
      (patch.includeSubfolders !== undefined && patch.includeSubfolders !== s.settings.includeSubfolders) ||
      (patch.skipRejected !== undefined && patch.skipRejected !== s.settings.skipRejected) ||
      (patch.includeVideos !== undefined && patch.includeVideos !== s.settings.includeVideos);
    if (scopeChanged && s.source && !s.importing) get().openSource(s.source);
    const dedupeChanged =
      patch.destRoot !== undefined || patch.folderTpl !== undefined || patch.nameTpl !== undefined;
    window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      api.saveSettings(get().settings);
      if (dedupeChanged) {
        const st = get().settings;
        api.setDedupeConfig(st.destRoot, st.folderTpl, st.nameTpl);
      }
    }, 300);
  },

  loadSettings: async () => {
    const saved = await api.getSettings();
    const settings = { ...defaultSettings, ...(saved ?? {}) };
    set({ settings, settingsLoaded: true });
    await api.setDedupeConfig(settings.destRoot, settings.folderTpl, settings.nameTpl);
  },

  setDestIndex: (destIndex) => {
    if (destIndex.root === get().settings.destRoot) set({ destIndex });
  },

  startImport: async (ids) => {
    const s = get();
    const { destRoot, folderTpl, nameTpl, paranoid } = s.settings;
    if (!destRoot) return;
    const itemIds = ids ?? [...s.selected].sort((a, b) => a - b);
    if (itemIds.length === 0) return;
    const outcomes = new Map(s.outcomes);
    itemIds.forEach((i) => outcomes.delete(i));
    set({ importing: true, progress: null, summary: null, importError: null, outcomes, importRoot: destRoot, importedDirs: {}, importedFiles: [] });
    try {
      await api.startImport({ destRoot, folderTpl, nameTpl, itemIds, paranoid });
    } catch (e) {
      set({ importing: false, importError: String(e) });
    }
  },

  setProgress: (progress) => set({ progress }),

  recordOutcome: (itemId, outcome) => {
    const outcomes = new Map(get().outcomes);
    const prev = outcomes.get(itemId);
    // An item is only as good as its worst file.
    const rank: Record<ItemOutcome, number> = { failed: 3, imported: 2, already_present: 1, skipped: 0 };
    if (!prev || rank[outcome.kind] > rank[prev.outcome]) {
      outcomes.set(itemId, { outcome: outcome.kind, error: outcome.kind === "failed" ? outcome.error : undefined });
    }
    if (outcome.kind === "imported" || outcome.kind === "already_present") {
      const dir = outcome.dest.replace(/[/\\][^/\\]*$/, "");
      const importedDirs = { ...get().importedDirs, [dir]: (get().importedDirs[dir] ?? 0) + 1 };
      const importedFiles = outcome.kind === "imported" ? [...get().importedFiles, outcome.dest] : get().importedFiles;
      set({ outcomes, importedDirs, importedFiles });
    } else set({ outcomes });
  },

  finishImport: (summary) => {
    const s = get();
    // Imported items are now duplicates; deselect everything that went through cleanly.
    const selected = new Set(s.selected);
    for (const [id, o] of s.outcomes) if (o.outcome !== "failed") selected.delete(id);
    set({ importing: false, summary, selected });
  },

  dismissSummary: () => set({ summary: null }),

  /** Load Immich settings and check the connection once. */
  loadImmich: async () => {
    const view = await api.getImmichSettings();
    const { hasKey, requiredPermission: _p, ...settings } = view;
    set({ immich: settings });
    if (!settings.enabled || !hasKey || !settings.url) {
      set({ immichStatus: { state: "off" } });
      return;
    }
    set({ immichStatus: { state: "testing" } });
    const r = await api.testImmich(settings, null);
    set({ immichStatus: r.ok ? { state: "ok", version: r.version } : { state: "error", error: r.error ?? "Unknown error" } });
  },

  setImmichStatus: (immichStatus) => {
    // Keep the version from the last successful test.
    const prev = get().immichStatus;
    if (immichStatus.state === "ok" && prev.state === "ok") return;
    set({ immichStatus });
  },

  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),

  markThumbFailed: (id) => {
    const items = get().items.slice();
    if (items[id]) items[id] = { ...items[id], thumbFailed: true };
    set({ items });
  },
}));
