import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { CheckCircle2, CircleAlert, FolderOpen, X } from "lucide-react";
import { api, formatBytes, formatDuration } from "../api";
import { useStore } from "../store";
import { Overlay } from "./ui";

export default function ImportProgressDialog() {
  const importing = useStore((s) => s.importing);
  const progress = useStore((s) => s.progress);
  const summary = useStore((s) => s.summary);
  const showImported = useStore((s) => s.showImported);
  const outcomes = useStore((s) => s.outcomes);
  const items = useStore((s) => s.items);
  const destRoot = useStore((s) => s.settings.destRoot);
  const startImport = useStore((s) => s.startImport);
  const dismiss = useStore((s) => s.dismissSummary);

  if (!importing && !summary) return null;

  if (importing) {
    const p = progress;
    const frac = p && p.bytesTotal > 0 ? p.bytesDone / p.bytesTotal : 0;
    const eta = p && p.bytesPerSec > 0 ? (p.bytesTotal - p.bytesDone) / p.bytesPerSec : NaN;
    return (
      <Overlay>
        <h2 className="text-base font-semibold">Importing</h2>
        <p className="mt-1 truncate text-text-secondary">{p?.current ? `Copying and verifying ${p.current}` : "Preparing…"}</p>
        <div className="mt-4 h-1.5 overflow-hidden rounded-full bg-bg-primary">
          <div className="h-full rounded-full bg-accent transition-[width] duration-200" style={{ width: `${frac * 100}%` }} />
        </div>
        <div className="mt-2 flex justify-between text-[12px] text-text-secondary">
          <span>
            {p?.filesDone ?? 0} / {p?.filesTotal ?? 0} files · {formatBytes(p?.bytesDone ?? 0)} of {formatBytes(p?.bytesTotal ?? 0)}
          </span>
          <span>
            {p ? `${formatBytes(p.bytesPerSec)}/s · ${formatDuration(eta)} left` : ""}
          </span>
        </div>
        <div className="mt-5 flex justify-end">
          <button className="rounded-md bg-surface px-4 py-1.5 hover:bg-card-active" onClick={() => api.cancelImport()}>
            Cancel
          </button>
        </div>
      </Overlay>
    );
  }

  const s = summary!;
  const failed = [...outcomes.entries()].filter(([, o]) => o.outcome === "failed");
  return (
    <Overlay>
      <div className="flex items-start gap-3">
        {s.failed > 0 ? (
          <CircleAlert className="mt-0.5 shrink-0 text-danger" size={22} />
        ) : (
          <CheckCircle2 className="mt-0.5 shrink-0 text-success" size={22} />
        )}
        <div className="flex-1">
          <h2 className="text-base font-semibold">
            {s.cancelled ? "Import cancelled" : s.failed > 0 ? "Import finished with errors" : "Import complete"}
          </h2>
          <p className="mt-1 text-text-secondary">
            {formatBytes(s.bytes)} in {formatDuration(s.seconds)}. Every copied file was verified by hash.
          </p>
        </div>
        <button className="text-text-secondary hover:text-text-primary" onClick={dismiss}>
          <X size={18} />
        </button>
      </div>
      <div className="mt-4 grid grid-cols-4 gap-2 text-center">
        <Stat n={s.imported} label="Imported" />
        <Stat n={s.alreadyPresent} label="Already there" />
        <Stat n={s.skipped} label="Skipped" />
        <Stat n={s.failed} label="Failed" danger={s.failed > 0} />
      </div>
      {failed.length > 0 && (
        <ul className="mt-4 max-h-40 space-y-1 overflow-y-auto rounded-md bg-bg-primary p-2 text-[12px]">
          {failed.map(([id, o]) => (
            <li key={id}>
              <span className="font-medium">{items[id]?.name}</span>
              <span className="text-text-secondary"> — {o.error}</span>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-5 flex justify-end gap-2">
        {destRoot && (
          <button
            className="flex items-center gap-2 rounded-md bg-surface px-4 py-1.5 hover:bg-card-active"
            onClick={() => revealItemInDir(destRoot)}
          >
            <FolderOpen size={14} /> Show in folder
          </button>
        )}
        {s.failedItems.length > 0 && (
          <button
            className="rounded-md bg-surface px-4 py-1.5 hover:bg-card-active"
            onClick={() => startImport(s.failedItems)}
          >
            Retry failed
          </button>
        )}
        {showImported && s.imported + s.alreadyPresent > 0 ? (
          <button
            className="rounded-md bg-accent px-4 py-1.5 font-semibold text-button-text"
            onClick={() => {
              dismiss();
              showImported();
            }}
          >
            Show in Immage
          </button>
        ) : (
          <button className="rounded-md bg-accent px-4 py-1.5 font-semibold text-button-text" onClick={dismiss}>
            Done
          </button>
        )}
      </div>
    </Overlay>
  );
}


function Stat({ n, label, danger }: { n: number; label: string; danger?: boolean }) {
  return (
    <div className="rounded-md bg-surface py-2">
      <div className={`text-lg font-semibold ${danger ? "text-danger" : ""}`}>{n}</div>
      <div className="text-[11px] text-text-secondary">{label}</div>
    </div>
  );
}
