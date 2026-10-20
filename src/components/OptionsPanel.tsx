import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Cloud, CloudOff, Database, FolderInput, Loader2, RefreshCw, Settings } from "lucide-react";
import { api, formatBytes } from "../api";
import { useStore } from "../store";
import { Section, Toggle } from "./ui";

const FOLDER_PRESETS = [
  { label: "Year / Month name", value: "{YYYY}/{MMMM}", hint: "2026/October" },
  { label: "Year / Month", value: "{YYYY}/{MM}", hint: "2026/10" },
  { label: "Year / Date", value: "{YYYY}/{YYYY}-{MM}-{DD}", hint: "2026/2026-10-05" },
  { label: "Year / Month / Day", value: "{YYYY}/{MM}/{DD}", hint: "2026/10/05" },
  { label: "Flat (no folders)", value: "", hint: "" },
];

const NAME_PRESETS = [
  { label: "Original name", value: "{original}" },
  { label: "Date_Time", value: "{YYYY}{MM}{DD}_{HH}{mm}{ss}" },
  { label: "Date-Original", value: "{YYYY}{MM}{DD}-{original}" },
  { label: "Date_Sequence", value: "{YYYY}-{MM}-{DD}_{seq}" },
];

const TOKENS = "{YYYY} {YY} {MMMM} {MMM} {MM} {DD} {HH} {mm} {ss} {camera} {make} {original} {seq}";

export default function OptionsPanel() {
  const settings = useStore((s) => s.settings);
  const update = useStore((s) => s.updateSettings);
  const selected = useStore((s) => s.selected);
  const items = useStore((s) => s.items);
  const importing = useStore((s) => s.importing);
  const startImport = useStore((s) => s.startImport);
  const importError = useStore((s) => s.importError);
  const openSettings = useStore((s) => s.openSettings);
  const firstSelected = useMemo(() => (selected.size ? Math.min(...selected) : null), [selected]);
  const [example, setExample] = useState("");

  useEffect(() => {
    const t = setTimeout(() => {
      api.examplePath(settings.folderTpl, settings.nameTpl, firstSelected).then((e) => setExample(e.path));
    }, 120);
    return () => clearTimeout(t);
  }, [settings.folderTpl, settings.nameTpl, firstSelected, items.length]);

  const chooseDest = async () => {
    const dir = await open({ directory: true, title: "Import into", defaultPath: settings.destRoot ?? undefined });
    if (typeof dir === "string") setDest(dir);
  };

  const setDest = (dir: string) => {
    const recentDests = [dir, ...settings.recentDests.filter((d) => d !== dir)].slice(0, 8);
    update({ destRoot: dir, recentDests });
  };

  const selectedBytes = useMemo(() => {
    let n = 0;
    for (const id of selected) n += items[id]?.size ?? 0;
    return n;
  }, [selected, items]);

  const folderPreset = FOLDER_PRESETS.find((p) => p.value === settings.folderTpl);
  const namePreset = NAME_PRESETS.find((p) => p.value === settings.nameTpl);
  // Wait until duplicate detection has finished for the selection.
  const pendingSelected = useMemo(() => {
    let n = 0;
    for (const id of selected) if (items[id]?.status === "pending") n++;
    return n;
  }, [selected, items]);
  const canImport = !!settings.destRoot && selected.size > 0 && !importing && pendingSelected === 0;
  const sep = example.includes("\\") ? "\\" : "/";

  return (
    <aside className="relative flex w-80 shrink-0 flex-col bg-bg-secondary">
      <button
        title="Settings"
        onClick={openSettings}
        className="absolute right-3 top-3.5 z-10 rounded-md p-1.5 text-text-secondary transition-colors hover:bg-card-active hover:text-text-primary"
      >
        <Settings size={16} />
      </button>
      <div className="flex-1 space-y-6 overflow-y-auto px-4 pb-4 pt-5">
        <Section title="Destination">
          <div className="flex gap-2">
            <select
              className="min-w-0 flex-1 truncate rounded-md border border-border-color bg-surface px-2 py-1.5 outline-none focus:border-text-secondary"
              value={settings.destRoot ?? ""}
              onChange={(e) => (e.target.value === "__browse" ? chooseDest() : setDest(e.target.value))}
            >
              {!settings.destRoot && <option value="">Choose a folder…</option>}
              {settings.recentDests.map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
              <option value="__browse">Browse…</option>
            </select>
            <IconButton title="Browse" onClick={chooseDest}>
              <FolderInput size={15} />
            </IconButton>
          </div>
          <DestStatus />
        </Section>

        <Section title="Folder structure">
          <select
            className="w-full rounded-md border border-border-color bg-surface px-2 py-1.5 outline-none"
            value={folderPreset ? settings.folderTpl : "__custom"}
            onChange={(e) => e.target.value !== "__custom" && update({ folderTpl: e.target.value })}
          >
            {FOLDER_PRESETS.map((p) => (
              <option key={p.label} value={p.value}>
                {p.label}
              </option>
            ))}
            <option value="__custom">Custom…</option>
          </select>
          <TemplateInput value={settings.folderTpl} onChange={(v) => update({ folderTpl: v })} placeholder="{YYYY}/{MMMM}" />
        </Section>

        <Section title="File naming">
          <select
            className="w-full rounded-md border border-border-color bg-surface px-2 py-1.5 outline-none"
            value={namePreset ? settings.nameTpl : "__custom"}
            onChange={(e) => e.target.value !== "__custom" && update({ nameTpl: e.target.value })}
          >
            {NAME_PRESETS.map((p) => (
              <option key={p.label} value={p.value}>
                {p.label}
              </option>
            ))}
            <option value="__custom">Custom…</option>
          </select>
          <TemplateInput value={settings.nameTpl} onChange={(v) => update({ nameTpl: v })} placeholder="{original}" />
          <p className="text-[11px] leading-relaxed text-text-secondary">{TOKENS}</p>
        </Section>

        <div className="rounded-md bg-surface px-3 py-2.5">
          <div className="section-title mb-1">Example</div>
          <div className="font-mono text-[11px] text-text-primary [overflow-wrap:anywhere]">
            {settings.destRoot ? <span className="text-text-secondary">{wrapAtSeparators(settings.destRoot + sep, sep)}</span> : null}
            {wrapAtSeparators(example, sep)}
          </div>
        </div>

        <Section title="Duplicates">
          <Toggle
            label="Import duplicates"
            hint="Import photos found in Immich, or imported before to a folder that still has them. Photos already in the destination are never copied twice."
            checked={settings.importDuplicates}
            onChange={(v) => update({ importDuplicates: v })}
          />
          <Toggle
            label="Import previously deleted"
            hint="Photos imported before whose copy is gone and that aren't in your library or Immich were probably deleted on purpose. Turn on to import them again."
            checked={settings.importDeleted}
            onChange={(v) => update({ importDeleted: v })}
          />
          <Toggle
            label="Paranoid verification"
            hint="Read every source file twice and compare, to catch flaky card reads. Slower."
            checked={settings.paranoid}
            onChange={(v) => update({ paranoid: v })}
          />
          <ImmichStatusLine />
        </Section>
      </div>

      <div className="space-y-2 border-t border-border-color p-4">
        {importError && <p className="text-[12px] text-danger">{importError}</p>}
        <div className="flex justify-between text-text-secondary">
          <span>{selected.size} photos</span>
          <span>{formatBytes(selectedBytes)}</span>
        </div>
        <button
          disabled={!canImport}
          onClick={() => startImport()}
          className="w-full rounded-md bg-accent py-2.5 font-semibold text-button-text transition-colors hover:opacity-90 disabled:cursor-not-allowed disabled:bg-card-active disabled:text-text-secondary"
        >
          {importing ? "Importing…" : pendingSelected > 0 ? `Checking ${pendingSelected}…` : "Import"}
        </button>
      </div>
    </aside>
  );
}

/** Allow line breaks only after path separators, not mid-word. */
function wrapAtSeparators(path: string, sep: string) {
  return path.split(sep).map((part, i, all) => (
    <span key={i} className="whitespace-nowrap">
      {part}
      {i < all.length - 1 && sep}
      {i < all.length - 1 && <wbr />}
    </span>
  ));
}

function ImmichStatusLine() {
  const status = useStore((s) => s.immichStatus);
  const immich = useStore((s) => s.immich);
  const openSettings = useStore((s) => s.openSettings);
  const loadImmich = useStore((s) => s.loadImmich);
  let icon = <CloudOff size={12} />;
  let text = "Immich: not set up";
  let tone = "text-text-secondary";
  if (immich?.enabled) {
    if (status.state === "testing") {
      icon = <Loader2 size={12} className="animate-spin" />;
      text = "Immich: connecting…";
    } else if (status.state === "ok") {
      icon = <Cloud size={12} />;
      text = `Immich: connected${status.version ? ` · v${status.version}` : ""}`;
    } else if (status.state === "error") {
      text = `Immich: ${status.error}`;
      tone = "text-danger";
    }
  }
  return (
    <div className={`flex items-start gap-2 pt-1 text-[11px] ${tone}`}>
      <span className="mt-px">{icon}</span>
      <span className="flex-1">{text}</span>
      {status.state === "error" && immich?.enabled && (
        <button className="text-text-secondary hover:text-text-primary" onClick={() => loadImmich()}>
          Retry
        </button>
      )}
      <button className="text-text-secondary hover:text-text-primary" onClick={openSettings}>
        {immich?.enabled ? "Settings" : "Set up"}
      </button>
    </div>
  );
}

function DestStatus() {
  const destIndex = useStore((s) => s.destIndex);
  const destRoot = useStore((s) => s.settings.destRoot);
  if (!destRoot) return <p className="text-[11px] text-text-secondary">Choose where photos will be copied to.</p>;
  const busy = destIndex && (destIndex.phase === "walking" || destIndex.phase === "hashing");
  let text = "";
  if (!destIndex) text = "Indexing library…";
  else if (destIndex.phase === "walking") text = `Indexing library… ${destIndex.done.toLocaleString()} photos`;
  else if (destIndex.phase === "hashing") text = `Hashing library ${destIndex.done.toLocaleString()} / ${destIndex.total.toLocaleString()}`;
  else if (destIndex.phase === "ready") text = `${destIndex.total.toLocaleString()} photos in library`;
  else if (destIndex.phase === "missing") text = "Folder not found";
  else text = destIndex.error ?? "Index error";
  return (
    <div className="flex items-center gap-2 text-[11px] text-text-secondary">
      {busy || !destIndex ? <Loader2 size={12} className="animate-spin" /> : <Database size={12} />}
      <span className="flex-1">{text}</span>
      <button
        className="flex items-center gap-1 hover:text-text-primary disabled:opacity-40"
        disabled={!!busy}
        title="Re-scan and hash every photo in the destination"
        onClick={() => api.rebuildDestIndex()}
      >
        <RefreshCw size={11} /> Rebuild
      </button>
    </div>
  );
}


function TemplateInput({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  return (
    <input
      value={value}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(e) => onChange(e.target.value)}
      className="w-full rounded-md border border-border-color bg-bg-primary px-2 py-1.5 font-mono text-[12px] outline-none focus:border-text-secondary"
    />
  );
}

function IconButton({ children, ...props }: React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button
      {...props}
      className="flex items-center justify-center rounded-md border border-border-color bg-surface px-2.5 text-text-secondary hover:bg-card-active hover:text-text-primary"
    >
      {children}
    </button>
  );
}

