import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { CardSim, ChevronRight, Folder, FolderOpen, HardDrive } from "lucide-react";
import { api, FolderEntry, formatBytes, Volume } from "../api";
import { useStore } from "../store";

export default function VolumeList() {
  const volumes = useStore((s) => s.volumes);
  const source = useStore((s) => s.source);
  const openSource = useStore((s) => s.openSource);
  const importing = useStore((s) => s.importing);
  const includeSubfolders = useStore((s) => s.settings.includeSubfolders);
  const skipRejected = useStore((s) => s.settings.skipRejected);
  const includeVideos = useStore((s) => s.settings.includeVideos);
  const update = useStore((s) => s.updateSettings);

  const chooseFolder = async () => {
    const dir = await open({ directory: true, title: "Import from folder" });
    if (typeof dir === "string") openSource(dir);
  };

  const onAnyVolume = source !== null && volumes.some((v) => source === v.mountPoint || source.startsWith(v.mountPoint + "/"));

  return (
    <aside className="flex w-64 shrink-0 flex-col bg-bg-secondary">
      <div className="px-4 pb-2 pt-5">
        <h2 className="section-title">Source</h2>
      </div>
      <div className="flex-1 space-y-1 overflow-y-auto px-2">
        {volumes.length === 0 && (
          <p className="px-2 py-3 text-text-secondary">Insert an SD card or connect a camera.</p>
        )}
        {volumes.map((v) => (
          <VolumeNode key={v.mountPoint} volume={v} disabled={importing} />
        ))}
        {source && !onAnyVolume && (
          <div className="flex items-start gap-3 rounded-md bg-card-active px-3 py-2.5">
            <FolderOpen size={18} className="mt-0.5 shrink-0" />
            <div className="min-w-0 break-all text-[12px]">{source}</div>
          </div>
        )}
      </div>
      <div className="space-y-2 border-t border-border-color p-3">
        <Check
          label="Include subfolders"
          title="When a folder is picked, also show photos in the folders inside it. Picking a whole card always includes everything."
          checked={includeSubfolders}
          disabled={importing}
          onChange={(v) => update({ includeSubfolders: v })}
        />
        <Check
          label="Include videos"
          title="Show and import video clips (MOV, MP4, MTS, AVI), including Sony's PRIVATE/M4ROOT folder."
          checked={includeVideos}
          disabled={importing}
          onChange={(v) => update({ includeVideos: v })}
        />
        <Check
          label="Skip _Rejected folders"
          title="Leave out FastRawViewer's _Rejected folders. Picking _Rejected itself still shows its photos."
          checked={skipRejected}
          disabled={importing}
          onChange={(v) => update({ skipRejected: v })}
        />
        <button
          onClick={chooseFolder}
          disabled={importing}
          className="flex w-full items-center justify-center gap-2 rounded-md bg-surface px-3 py-2 text-text-secondary transition-colors hover:bg-card-active hover:text-text-primary disabled:opacity-50"
        >
          <FolderOpen size={15} /> Choose folder…
        </button>
      </div>
    </aside>
  );
}

function VolumeNode({ volume: v, disabled }: { volume: Volume; disabled: boolean }) {
  const source = useStore((s) => s.source);
  const openSource = useStore((s) => s.openSource);
  const [expanded, setExpanded] = useState(false);
  const active = v.mountPoint === source;
  const Icon = v.hasDcim || v.removable ? CardSim : HardDrive;
  const used = v.total > 0 ? 1 - v.available / v.total : 0;

  return (
    <div>
      <div
        className={`group flex w-full items-start gap-1 rounded-md py-2.5 pl-1 pr-3 transition-colors ${
          active ? "bg-card-active" : "hover:bg-surface"
        } ${disabled ? "pointer-events-none opacity-50" : ""}`}
      >
        <Chevron expanded={expanded} onClick={() => setExpanded(!expanded)} />
        <button className="flex min-w-0 flex-1 items-start gap-2.5 text-left" onClick={() => openSource(v.mountPoint)}>
          <Icon size={18} className={`mt-0.5 shrink-0 ${active ? "text-accent" : "text-text-secondary"}`} />
          <div className="min-w-0 flex-1">
            <div className="truncate font-medium">{v.name}</div>
            <div className="mt-0.5 text-[11px] text-text-secondary">
              {formatBytes(v.total - v.available)} of {formatBytes(v.total)}
              {v.hasDcim && " · DCIM"}
            </div>
            <div className="mt-1.5 h-1 overflow-hidden rounded-full bg-bg-primary">
              <div className="h-full rounded-full bg-text-secondary/60" style={{ width: `${used * 100}%` }} />
            </div>
          </div>
        </button>
      </div>
      {expanded && <FolderChildren path={v.mountPoint} depth={1} disabled={disabled} />}
    </div>
  );
}

/** Lazily loaded subfolders of `path`. */
function FolderChildren({ path, depth, disabled }: { path: string; depth: number; disabled: boolean }) {
  const [folders, setFolders] = useState<FolderEntry[] | null>(null);
  useEffect(() => {
    let live = true;
    api.listFolders(path).then((f) => live && setFolders(f));
    return () => {
      live = false;
    };
  }, [path]);
  if (folders === null) {
    return <div className="py-1 text-[11px] text-text-secondary" style={{ paddingLeft: depth * 14 + 22 }}>…</div>;
  }
  if (folders.length === 0) return null;
  return (
    <>
      {folders.map((f) => (
        <FolderNode key={f.path} folder={f} depth={depth} disabled={disabled} />
      ))}
    </>
  );
}

function FolderNode({ folder: f, depth, disabled }: { folder: FolderEntry; depth: number; disabled: boolean }) {
  const source = useStore((s) => s.source);
  const openSource = useStore((s) => s.openSource);
  // Open the tree down to the picked folder.
  const [expanded, setExpanded] = useState(() => source?.startsWith(f.path + "/") ?? false);
  const active = source === f.path;

  return (
    <div>
      <div
        className={`flex items-center gap-1 rounded-md py-1 pr-2 transition-colors ${
          active ? "bg-card-active text-text-primary" : "text-text-secondary hover:bg-surface hover:text-text-primary"
        } ${disabled ? "pointer-events-none opacity-50" : ""}`}
        style={{ paddingLeft: depth * 14 }}
      >
        {f.hasChildren ? (
          <Chevron expanded={expanded} onClick={() => setExpanded(!expanded)} />
        ) : (
          <span className="w-5 shrink-0" />
        )}
        <button className="flex min-w-0 flex-1 items-center gap-2 text-left" onClick={() => openSource(f.path)}>
          <Folder size={14} className={`shrink-0 ${active ? "text-accent" : ""}`} />
          <span className="truncate text-[12px]">{f.name}</span>
          {f.photos > 0 && <span className="ml-auto text-[11px] text-text-secondary">{f.photos}</span>}
        </button>
      </div>
      {expanded && <FolderChildren path={f.path} depth={depth + 1} disabled={disabled} />}
    </div>
  );
}

function Chevron({ expanded, onClick }: { expanded: boolean; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="flex h-5 w-5 shrink-0 items-center justify-center rounded text-text-secondary hover:text-text-primary"
      title={expanded ? "Collapse" : "Show folders"}
    >
      <ChevronRight size={14} className={`transition-transform ${expanded ? "rotate-90" : ""}`} />
    </button>
  );
}

function Check({
  label,
  title,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  title: string;
  checked: boolean;
  disabled: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label title={title} className={`flex cursor-pointer items-center gap-2 px-1 text-[12px] text-text-secondary ${disabled ? "opacity-50" : ""}`}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="h-3.5 w-3.5 accent-[var(--app-accent)]"
      />
      {label}
    </label>
  );
}
