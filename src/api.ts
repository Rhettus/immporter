import { convertFileSrc, invoke as tauriInvoke, type InvokeArgs } from "@tauri-apps/api/core";

// The backend is a Tauri plugin, so the standalone app and apps embedding it
// call the same namespaced commands.
const invoke = <T>(cmd: string, args?: InvokeArgs) => tauriInvoke<T>(`plugin:immporter|${cmd}`, args);
import { listen, UnlistenFn } from "@tauri-apps/api/event";

export type FileStatus =
  | "pending"
  | "new"
  | "checking"
  | "checking_immich"
  | "deleted"
  | "duplicate"
  | "in_immich"
  | "in_destination";

export interface ImmichMatch {
  assetId: string;
  originalPath: string;
  /** Confirmed by SHA-1 of the file contents (uploaded assets). */
  verified: boolean;
}

export interface ImmichSettings {
  enabled: boolean;
  url: string;
  acceptInvalidCerts: boolean;
  matchPath: boolean;
  pathPrefix: string;
}

export interface ImmichSettingsView extends ImmichSettings {
  hasKey: boolean;
  requiredPermission: string;
}

export interface ImmichTestResult {
  ok: boolean;
  version: string | null;
  keyName: string | null;
  hasAssetRead: boolean;
  samplePath: string | null;
  error: string | null;
}

export type ImmichStatus =
  | { state: "off" }
  | { state: "testing" }
  | { state: "ok"; version?: string | null }
  | { state: "error"; error: string };

export interface Volume {
  name: string;
  mountPoint: string;
  total: number;
  available: number;
  removable: boolean;
  fileSystem: string;
  hasDcim: boolean;
}

export type MediaKind = "raw" | "jpeg" | "other" | "video" | "sidecar";

export interface FileView {
  name: string;
  size: number;
  kind: MediaKind;
  status: FileStatus;
  existing: string | null;
  immich: ImmichMatch | null;
}

export interface ItemView {
  id: number;
  name: string;
  dir: string;
  size: number;
  files: FileView[];
  capture: string | null;
  /** File modification time: stands in for the capture time until EXIF is read. */
  fileTime: string | null;
  dateSource: "exif" | "created" | "modified" | null;
  camera: string | null;
  thumb: string | null;
  thumbFailed: boolean;
  status: FileStatus;
  /** Kind of the primary file. */
  kind: MediaKind;
  durationSecs: number | null;
  width: number | null;
  height: number | null;
}

export interface ScanResult {
  gen: number;
  root: string;
  items: ItemView[];
}

export interface DestIndexEvent {
  root: string;
  phase: "walking" | "ready" | "hashing" | "missing" | "error";
  done: number;
  total: number;
  error?: string;
}

export interface ImportProgress {
  filesDone: number;
  filesTotal: number;
  bytesDone: number;
  bytesTotal: number;
  bytesPerSec: number;
  current: string;
}

export type Outcome =
  | { kind: "imported"; dest: string; hash: string }
  | { kind: "already_present"; dest: string; hash: string }
  | { kind: "skipped"; reason: string }
  | { kind: "failed"; error: string };

export interface ImportFileEvent {
  itemId: number;
  file: string;
  outcome: Outcome;
}

export interface Summary {
  imported: number;
  alreadyPresent: number;
  skipped: number;
  failed: number;
  failedItems: number[];
  bytes: number;
  seconds: number;
  cancelled: boolean;
}

export interface Settings {
  destRoot: string | null;
  recentDests: string[];
  folderTpl: string;
  nameTpl: string;
  importDuplicates: boolean;
  /** Re-import photos imported before whose copy is gone (not in the library or Immich). */
  importDeleted: boolean;
  paranoid: boolean;
  tileSize: number;
  /** Hide photos already in the library (destination, import history or Immich). */
  hideDuplicates: boolean;
  /** When a folder (not a whole card) is picked, include its subfolders. */
  includeSubfolders: boolean;
  /** Leave out FastRawViewer's `_Rejected` folders. */
  skipRejected: boolean;
  /** Show and import videos. */
  includeVideos: boolean;
  sortBy: SortBy;
  sortDesc: boolean;
}

export type SortBy = "capture" | "name" | "folder";

export interface FolderEntry {
  name: string;
  path: string;
  photos: number;
  hasChildren: boolean;
}

export const defaultSettings: Settings = {
  destRoot: null,
  recentDests: [],
  folderTpl: "{YYYY}/{MMMM}",
  nameTpl: "{original}",
  importDuplicates: false,
  importDeleted: false,
  paranoid: true,
  tileSize: 180,
  hideDuplicates: false,
  includeSubfolders: false,
  skipRejected: true,
  includeVideos: true,
  sortBy: "capture",
  sortDesc: false,
};

export const api = {
  devAutoloadSource: () => invoke<string | null>("dev_autoload_source"),
  listVolumes: () => invoke<Volume[]>("list_volumes"),
  scanSource: (path: string, includeSubfolders: boolean, skipRejected: boolean, includeVideos: boolean) =>
    invoke<ScanResult>("scan_source", { path, includeSubfolders, skipRejected, includeVideos }),
  saveVideoThumb: (gen: number, itemId: number, jpegBase64: string) =>
    invoke<void>("save_video_thumb", { gen, itemId, jpegBase64 }),
  listFolders: (path: string) => invoke<FolderEntry[]>("list_folders", { path }),
  closeSource: () => invoke<void>("close_source"),
  setVisible: (gen: number, start: number, end: number) => invoke<void>("set_visible", { gen, start, end }),
  getSettings: () => invoke<Partial<Settings> | null>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  setDedupeConfig: (destRoot: string | null, folderTpl: string, nameTpl: string) =>
    invoke<void>("set_dedupe_config", { args: { destRoot, folderTpl, nameTpl } }),
  rebuildDestIndex: () => invoke<void>("rebuild_dest_index"),
  examplePath: (folderTpl: string, nameTpl: string, itemId: number | null) =>
    invoke<{ path: string; fromItem: boolean }>("example_path", { folderTpl, nameTpl, itemId }),
  startImport: (opts: { destRoot: string; folderTpl: string; nameTpl: string; itemIds: number[]; paranoid: boolean }) =>
    invoke<void>("start_import", { opts }),
  cancelImport: () => invoke<void>("cancel_import"),
  getImmichSettings: () => invoke<ImmichSettingsView>("get_immich_settings"),
  saveImmichSettings: (settings: ImmichSettings, apiKey: string | null) =>
    invoke<void>("save_immich_settings", { settings, apiKey }),
  forgetImmichKey: () => invoke<void>("forget_immich_key"),
  testImmich: (settings: ImmichSettings, apiKey: string | null) =>
    invoke<ImmichTestResult>("test_immich", { settings, apiKey }),
};

export const thumbUrl = (key: string) => convertFileSrc(key, "thumb");

/** Streams a video of the current scan (read-only, range requests). */
const mediaBase = convertFileSrc("", "media");
export const mediaUrl = (gen: number, itemId: number) => `${mediaBase.replace(/\/$/, "")}/${gen}/${itemId}/0`;

/** Clip length: 0:32, 12:05, 1:02:03. */
export function formatClipLength(secs: number): string {
  const s = Math.round(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

/** "4K", "1080p", … from the frame size. */
export function resolutionLabel(w: number | null, h: number | null): string | null {
  if (!w || !h) return null;
  const short = Math.min(w, h);
  if (Math.max(w, h) >= 3840) return "4K";
  if (short >= 1080) return "1080p";
  if (short >= 720) return "720p";
  return `${short}p`;
}

export function on<T>(event: string, cb: (payload: T) => void): Promise<UnlistenFn> {
  return listen<T>(event, (e) => cb(e.payload));
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function formatDuration(secs: number): string {
  if (!isFinite(secs) || secs < 0) return "–";
  const s = Math.round(secs);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}
