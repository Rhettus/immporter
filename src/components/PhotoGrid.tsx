import { useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ArrowDown, ArrowUp, ChevronRight, Eye, EyeOff, Images, Loader2 } from "lucide-react";
import { api, formatBytes, ItemView, SortBy, Volume } from "../api";
import { Filter, useStore } from "../store";
import PhotoTile from "./PhotoTile";

const GAP = 8;
const LABEL_H = 34;

export default function PhotoGrid() {
  const items = useStore((s) => s.items);
  const gen = useStore((s) => s.gen);
  const source = useStore((s) => s.source);
  const scanning = useStore((s) => s.scanning);
  const scanError = useStore((s) => s.scanError);
  const filter = useStore((s) => s.filter);
  const setFilter = useStore((s) => s.setFilter);
  const selected = useStore((s) => s.selected);
  const tileSize = useStore((s) => s.settings.tileSize);
  const hideDuplicates = useStore((s) => s.settings.hideDuplicates);
  const updateSettings = useStore((s) => s.updateSettings);
  const selectAll = useStore((s) => s.selectAll);
  const selectNone = useStore((s) => s.selectNone);

  const scrollRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(800);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const sortBy = useStore((s) => s.settings.sortBy);
  const sortDesc = useStore((s) => s.settings.sortDesc);
  const volumes = useStore((s) => s.volumes);

  const visible = useMemo(() => {
    let v: ItemView[];
    if (filter === "new") v = items.filter((i) => i.status === "new" || i.status === "pending");
    else if (filter === "duplicates") v = items.filter((i) => i.status !== "new" && i.status !== "pending");
    else if (!hideDuplicates) v = items.slice();
    // Still-checking photos stay visible until they are known to be duplicates.
    else
      v = items.filter(
        (i) => i.status !== "duplicate" && i.status !== "in_immich" && i.status !== "in_destination" && i.status !== "deleted",
      );
    return sortItems(v, sortBy, sortDesc);
  }, [items, filter, hideDuplicates, sortBy, sortDesc]);
  const visibleIds = useMemo(() => visible.map((i) => i.id), [visible]);

  const counts = useMemo(() => {
    let fresh = 0;
    let dupes = 0;
    let checking = 0;
    for (const i of items) {
      if (i.status === "new") fresh++;
      else if (i.status === "duplicate" || i.status === "in_destination" || i.status === "in_immich" || i.status === "deleted") dupes++;
      else checking++;
    }
    return { fresh, dupes, checking };
  }, [items]);

  const selectedBytes = useMemo(() => {
    let n = 0;
    for (const id of selected) n += items[id]?.size ?? 0;
    return n;
  }, [selected, items]);

  const pad = 16;
  const cols = Math.max(1, Math.floor((width - pad * 2 + GAP) / (tileSize + GAP)));
  const cell = Math.floor((width - pad * 2 - GAP * (cols - 1)) / cols);
  const rows = Math.ceil(visible.length / cols);

  const virt = useVirtualizer({
    count: rows,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => cell + LABEL_H + GAP,
    overscan: 4,
  });
  useEffect(() => virt.measure(), [cell, virt]);

  // Tell the backend what is on screen so those previews load first.
  const range = virt.range;
  useEffect(() => {
    if (!range || visible.length === 0) return;
    // Sorted views aren't in id order: send the id span of what's on screen.
    const onScreen = visible.slice(range.startIndex * cols, (range.endIndex + 1) * cols).map((i) => i.id);
    if (onScreen.length === 0) return;
    const start = Math.min(...onScreen);
    const end = Math.max(...onScreen) + 1;
    const t = setTimeout(() => api.setVisible(gen, start, end), 60);
    return () => clearTimeout(t);
  }, [range?.startIndex, range?.endIndex, cols, gen, visible]);

  // ⌘A / ⌘D (Ctrl on Windows/Linux)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || (e.target as HTMLElement)?.tagName === "INPUT") return;
      if (e.key === "a") {
        e.preventDefault();
        selectAll(visibleIds);
      } else if (e.key === "d") {
        e.preventDefault();
        selectNone();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [visibleIds, selectAll, selectNone]);

  const FilterBtn = ({ value, label }: { value: Filter; label: string }) => (
    <button
      onClick={() => setFilter(value)}
      className={`rounded px-2.5 py-1 transition-colors ${
        filter === value ? "bg-card-active text-text-primary" : "text-text-secondary hover:text-text-primary"
      }`}
    >
      {label}
    </button>
  );

  return (
    <>
      <header className="flex h-12 shrink-0 items-center gap-4 border-b border-border-color bg-bg-secondary px-4">
        <div className="flex items-center gap-0.5 rounded-md bg-bg-primary p-0.5">
          <FilterBtn value="all" label={`All ${items.length}`} />
          <FilterBtn value="new" label={`New ${counts.fresh}`} />
          <FilterBtn value="duplicates" label={`Duplicates ${counts.dupes}`} />
        </div>
        <button
          onClick={() => updateSettings({ hideDuplicates: !hideDuplicates })}
          disabled={filter !== "all"}
          title={hideDuplicates ? "Show duplicates" : "Hide duplicates"}
          className={`flex items-center gap-1.5 rounded-md px-2 py-1 transition-colors disabled:opacity-30 ${
            hideDuplicates && filter === "all"
              ? "bg-card-active text-text-primary"
              : "text-text-secondary hover:text-text-primary"
          }`}
        >
          {hideDuplicates ? <EyeOff size={14} /> : <Eye size={14} />}
          {hideDuplicates ? `${counts.dupes} hidden` : "Hide duplicates"}
        </button>
        {counts.checking > 0 && !scanning && (
          <span className="flex items-center gap-1.5 text-text-secondary">
            <Loader2 size={13} className="animate-spin" /> Checking {counts.checking}
          </span>
        )}
        <div className="flex-1" />
        <div className="flex items-center gap-1 text-text-secondary">
          <select
            value={sortBy}
            onChange={(e) => updateSettings({ sortBy: e.target.value as SortBy })}
            title="Sort order"
            className="rounded-md border border-border-color bg-surface px-1.5 py-1 text-[12px] outline-none"
          >
            <option value="capture">Capture time</option>
            <option value="name">File name</option>
            <option value="folder">Folder</option>
          </select>
          <button
            onClick={() => updateSettings({ sortDesc: !sortDesc })}
            title={sortDesc ? "Descending" : "Ascending"}
            className="rounded-md p-1 hover:bg-card-active hover:text-text-primary"
          >
            {sortDesc ? <ArrowDown size={14} /> : <ArrowUp size={14} />}
          </button>
        </div>
        <span className="text-text-secondary">
          {selected.size} selected · {formatBytes(selectedBytes)}
        </span>
        <button className="text-text-secondary hover:text-text-primary" onClick={() => selectAll(visibleIds)}>
          All
        </button>
        <button className="text-text-secondary hover:text-text-primary" onClick={selectNone}>
          None
        </button>
        <input
          type="range"
          min={110}
          max={340}
          value={tileSize}
          onChange={(e) => updateSettings({ tileSize: Number(e.target.value) })}
          className="w-24 accent-[var(--app-accent)]"
          title="Thumbnail size"
        />
      </header>

      {source && <Breadcrumb source={source} volumes={volumes} count={visible.length} />}
      <div ref={scrollRef} className="relative flex-1 overflow-y-auto">
        {!source && (
          <Empty icon={<Images size={40} />} text="Select a card or folder on the left to see its photos." />
        )}
        {source && scanning && <Empty icon={<Loader2 size={32} className="animate-spin" />} text="Reading card…" />}
        {source && scanError && <Empty icon={<Images size={40} />} text={scanError} />}
        {source && !scanning && !scanError && items.length === 0 && (
          <Empty icon={<Images size={40} />} text="No photos found here." />
        )}
        {visible.length > 0 && (
          <div style={{ height: virt.getTotalSize() + pad * 2, position: "relative" }}>
            {virt.getVirtualItems().map((row) => (
              <div
                key={row.key}
                className="absolute left-0 flex"
                style={{ top: row.start + pad, left: pad, gap: GAP, height: cell + LABEL_H }}
              >
                {visible.slice(row.index * cols, row.index * cols + cols).map((item) => (
                  <PhotoTile key={item.id} item={item} size={cell} visibleIds={visibleIds} />
                ))}
              </div>
            ))}
          </div>
        )}
      </div>
    </>
  );
}

function Empty({ icon, text }: { icon: React.ReactNode; text: string }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 text-text-secondary">
      {icon}
      <p className="max-w-sm text-center">{text}</p>
    </div>
  );
}

const byName = (a: ItemView, b: ItemView) => a.name.localeCompare(b.name, undefined, { numeric: true });

function sortItems(v: ItemView[], by: SortBy, desc: boolean): ItemView[] {
  const cmp =
    by === "name"
      ? byName
      : by === "folder"
        ? (a: ItemView, b: ItemView) => a.dir.localeCompare(b.dir, undefined, { numeric: true }) || byName(a, b)
        : // Capture time; the file time stands in until EXIF is read. ISO strings sort correctly.
          (a: ItemView, b: ItemView) => {
            const ta = a.capture ?? a.fileTime ?? "";
            const tb = b.capture ?? b.fileTime ?? "";
            return ta < tb ? -1 : ta > tb ? 1 : byName(a, b);
          };
  v.sort(desc ? (a, b) => cmp(b, a) : cmp);
  return v;
}

/** `OM SYSTEM › DCIM › 100OMSYS › _Selected` */
function Breadcrumb({ source, volumes, count }: { source: string; volumes: Volume[]; count: number }) {
  const vol = volumes
    .filter((v) => source === v.mountPoint || source.startsWith(v.mountPoint + "/"))
    .sort((a, b) => b.mountPoint.length - a.mountPoint.length)[0];
  const parts = vol
    ? [vol.name, ...source.slice(vol.mountPoint.length).split(/[\\/]/).filter(Boolean)]
    : source.split(/[\\/]/).filter(Boolean).slice(-3);
  return (
    <div className="flex h-7 shrink-0 items-center gap-1 border-b border-border-color bg-bg-primary px-4 text-[11px] text-text-secondary">
      {parts.map((p, i) => (
        <span key={i} className="flex items-center gap-1">
          {i > 0 && <ChevronRight size={11} />}
          <span className={i === parts.length - 1 ? "text-text-primary" : ""}>{p}</span>
        </span>
      ))}
      <span className="ml-2">· {count.toLocaleString()} shown</span>
    </div>
  );
}
