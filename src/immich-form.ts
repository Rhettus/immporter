// The Immich settings form's logic, shared by the importer's own dialog and
// by an embedding app's settings (Immage's Preferences).
import type { ImmichSettings } from "./api";

export const EMPTY: ImmichSettings = { enabled: false, url: "", acceptInvalidCerts: false, matchPath: false, pathPrefix: "" };

/** Plain http to something that isn't obviously on the local network. */
export function insecureRemote(url: string): boolean {
  const m = url.trim().match(/^http:\/\/([^/:]+)/i);
  if (!m) return false;
  const host = m[1].toLowerCase();
  return !(
    host === "localhost" ||
    host.endsWith(".local") ||
    host.endsWith(".lan") ||
    host.endsWith(".home.arpa") ||
    /^(127\.|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/.test(host) ||
    !host.includes(".")
  );
}

/**
 * Suggest the path prefix: strip the folder levels and file name the
 * template produces from a real Immich path. With a 2-level template
 * (`2026/October/X.JPG`), `/mnt/photos/2024/May/P1.ORF` → `/mnt/photos`.
 */
export function suggestPrefix(samplePath: string, exampleRel: string): string | null {
  const levels = exampleRel.split("/").length; // folders + file
  const parts = samplePath.replace(/\\/g, "/").split("/");
  if (parts.length <= levels) return null;
  return parts.slice(0, parts.length - levels).join("/") || "/";
}
