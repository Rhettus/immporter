import { useEffect } from "react";
import { api, DestIndexEvent, ImmichStatus, ImportFileEvent, ImportProgress, ItemView, on, Summary, Volume } from "./api";
import { isOnVolume, useStore } from "./store";
import VolumeList from "./components/VolumeList";
import PhotoGrid from "./components/PhotoGrid";
import OptionsPanel from "./components/OptionsPanel";
import ImportProgressDialog from "./components/ImportProgress";
import SettingsDialog from "./components/SettingsDialog";
import VideoThumbs from "./components/VideoThumbs";

/** The importer itself: its event wiring and three columns. The standalone
 * app adds its settings dialog; an embedding app (embed.tsx) may use its own. */
export function Shell() {
  useEffect(() => {
    const s = useStore.getState;
    s().loadSettings();
    s().loadImmich();
    api.devAutoloadSource().then((src) => {
      if (src && !s().source) s().openSource(src);
    });
    void s().refreshSource();
    const subs = [
      on<Volume[]>("volumes-changed", (v) => {
        const src = s().source;
        const wasVolume = src !== null && s().volumes.some((x) => isOnVolume(src, x.mountPoint));
        useStore.setState((st) => ({ volumes: v, refreshSeq: st.refreshSeq + 1 }));
        // The card was ejected (source is the card or a folder on it): drop the
        // grid, unless an import is reporting on it.
        if (src && wasVolume && !v.some((x) => isOnVolume(src, x.mountPoint)) && !s().importing) {
          useStore.setState({
            source: null,
            sourceRoot: null,
            items: [],
            selected: new Set(),
            highlighted: new Set(),
            touched: new Set(),
            anchor: null,
            outcomes: new Map(),
            summary: null,
            scanError: null,
          });
          api.closeSource();
        }
      }),
      on<{ gen: number; items: ItemView[] }>("items-updated", (p) => s().applyUpdates(p.gen, p.items)),
      on<DestIndexEvent>("dest-index", (e) => s().setDestIndex(e)),
      on<ImportProgress>("import-progress", (p) => s().setProgress(p)),
      on<ImportFileEvent>("import-file", (e) => s().recordOutcome(e.itemId, e.outcome)),
      on<Summary>("import-done", (sm) => s().finishImport(sm)),
      on<ImmichStatus>("immich-status", (st) => s().setImmichStatus(st)),
    ];
    return () => {
      subs.forEach((p) => p.then((un) => un()));
      if (!useStore.getState().importing) api.closeSource();
    };
  }, []);

  return (
    <div className="flex h-full bg-bg-primary text-text-primary">
      <VolumeList />
      <main className="flex min-w-0 flex-1 flex-col border-x border-border-color">
        <PhotoGrid />
      </main>
      <OptionsPanel />
      <ImportProgressDialog />
      <VideoThumbs />
    </div>
  );
}

export default function App() {
  return (
    <>
      <Shell />
      <SettingsDialog />
    </>
  );
}
