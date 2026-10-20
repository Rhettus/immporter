// The importer inside another app (Immage): its whole UI as one component,
// styled without touching the host page, with hooks for the host's own
// settings and for what follows an import.
import { useEffect } from "react";
import { Shell } from "./App";
import SettingsDialog from "./components/SettingsDialog";
import { useStore } from "./store";
import type { Summary } from "./api";
import "./embed.css";

export type ImportResult = {
  summary: Summary;
  /** The destination root of the import. */
  root: string | null;
  /** Each folder files landed in (or were found already in), with its count. */
  dirs: Record<string, number>;
  /** The files copied, in the order they were (oldest capture first). */
  files: string[];
};

export function ImmporterView({ onOpenSettings, onImportDone, onShowImported }: {
  /** Open the host's settings instead of the importer's own dialog. */
  onOpenSettings?: () => void;
  onImportDone?: (r: ImportResult) => void;
  /** Adds "Show in <host>" to the import summary. */
  onShowImported?: (r: ImportResult) => void;
}) {
  const result = (): ImportResult => {
    const s = useStore.getState();
    return { summary: s.summary!, root: s.importRoot, dirs: s.importedDirs, files: s.importedFiles };
  };
  useEffect(() => {
    useStore.setState({
      openSettings: onOpenSettings ?? (() => useStore.setState({ settingsOpen: true })),
      showImported: onShowImported ? () => onShowImported(result()) : null,
    });
  }, [onOpenSettings, onShowImported]);
  useEffect(
    () =>
      useStore.subscribe((s, prev) => {
        if (s.summary && s.summary !== prev.summary) onImportDone?.(result());
      }),
    [onImportDone],
  );
  return (
    <div className="immporter">
      <Shell />
      {!onOpenSettings && <SettingsDialog />}
    </div>
  );
}

/** Read the Immich settings again, after the host's settings changed them. */
export const reloadImmich = () => useStore.getState().loadImmich();

/** Whether an import is running, for a host that must not close the view. */
export const useImporting = () => useStore((s) => s.importing);

export { api } from "./api";
export type { ImmichSettings, ImmichSettingsView, ImmichTestResult } from "./api";
