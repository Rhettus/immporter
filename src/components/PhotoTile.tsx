import { memo } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { AlertTriangle, Check, CircleAlert, Clock, Cloud, Copy, Film, ImageOff, Loader2, Play, Trash2 } from "lucide-react";
import { formatClipLength, ItemView, resolutionLabel, thumbUrl } from "../api";
import { selectable, useStore } from "../store";

interface Props {
  item: ItemView;
  size: number;
  visibleIds: number[];
}

function formatCapture(iso: string | null) {
  if (!iso) return "";
  const d = new Date(iso);
  return d.toLocaleString(undefined, { day: "numeric", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" });
}

function PhotoTile({ item, size, visibleIds }: Props) {
  const isSelected = useStore((s) => s.selected.has(item.id));
  const policy = useStore((s) => s.settings);
  const outcome = useStore((s) => s.outcomes.get(item.id));
  const toggle = useStore((s) => s.toggle);
  const immichUrl = useStore((s) => s.immich?.url);
  const canSelect = selectable(item, policy);
  const dimmed =
    item.status === "in_destination" ||
    item.status === "checking" ||
    item.status === "checking_immich" ||
    ((item.status === "duplicate" || item.status === "in_immich" || item.status === "deleted") && !canSelect);
  const immichMatch = item.files.find((f) => f.immich)?.immich ?? null;

  const exts = item.files.map((f) => f.name.split(".").pop()?.toUpperCase()).join("+");
  const existing = item.files.find((f) => f.existing)?.existing;
  const title = [
    `${item.dir ? item.dir + "/" : ""}${item.name}`,
    item.camera,
    item.status === "in_destination" && existing ? `Already in destination: ${existing}` : null,
    item.status === "duplicate" && existing ? `Previously imported to: ${existing}` : null,
    item.status === "deleted"
      ? `Imported before${existing ? ` to ${existing}` : ""}, but that copy is gone and it isn't in your library or Immich. Probably deleted on purpose; turn on "Import previously deleted" to bring it back.`
      : null,
    item.status === "in_immich" && immichMatch
      ? `In Immich: ${immichMatch.originalPath}\nMatched by ${immichMatch.verified ? "file contents (SHA-1)" : "name, size and capture time"}\nDouble-click to open in Immich`
      : null,
    item.status !== "in_immich" && immichMatch
      ? `Also in Immich: ${immichMatch.originalPath}\nDouble-click to open in Immich`
      : null,
    ...(["duplicate", "in_immich", "in_destination"].includes(item.status)
      ? item.files
          .filter((f) => f.status === "new")
          .map((f) => `${f.name} differs from the library copy (e.g. edit metadata written into it); the RAW is already there, so it is skipped`)
      : []),
    outcome?.error,
  ]
    .filter(Boolean)
    .join("\n");

  return (
    <div
      className="group flex flex-col"
      style={{ width: size }}
      title={title}
      onClick={(e) => canSelect && toggle(item.id, e.shiftKey, visibleIds)}
      onDoubleClick={() => immichMatch && immichUrl && openUrl(`${immichUrl.replace(/\/+$/, "")}/photos/${immichMatch.assetId}`)}
    >
      <div
        className={`relative overflow-hidden rounded-md bg-surface transition-all ${
          isSelected ? "ring-2 ring-accent" : "ring-1 ring-border-color"
        } ${canSelect ? "cursor-pointer" : "cursor-default"}`}
        style={{ width: size, height: size }}
      >
        {item.thumb ? (
          <img
            src={thumbUrl(item.thumb)}
            draggable={false}
            loading="lazy"
            decoding="async"
            className={`h-full w-full object-contain transition-opacity ${dimmed ? "opacity-35" : ""}`}
          />
        ) : (
          <div className="flex h-full w-full items-center justify-center text-text-secondary">
            {item.thumbFailed ? (
              item.kind === "video" ? <Film size={26} /> : <ImageOff size={22} />
            ) : (
              <Loader2 size={18} className="animate-spin opacity-50" />
            )}
          </div>
        )}

        {canSelect && (
          <div
            className={`absolute left-2 top-2 flex h-5 w-5 items-center justify-center rounded border transition-opacity ${
              isSelected
                ? "border-accent bg-accent text-button-text"
                : "border-text-secondary/70 bg-black/40 opacity-0 group-hover:opacity-100"
            }`}
          >
            {isSelected && <Check size={13} strokeWidth={3} />}
          </div>
        )}

        <div className="absolute right-2 top-2 flex gap-1">
          {item.status === "in_destination" && <Badge icon={<Copy size={11} />} text="In library" />}
          {item.status === "duplicate" && <Badge icon={<Copy size={11} />} text="Imported" />}
          {item.status === "deleted" && <Badge icon={<Trash2 size={11} />} text="Deleted" />}
          {item.status !== "in_immich" && immichMatch && (
            <span title="Also in Immich" className="flex items-center rounded bg-black/70 px-1.5 py-0.5 text-text-primary">
              <Cloud size={11} />
            </span>
          )}
          {item.status === "checking" && <Badge icon={<Loader2 size={11} className="animate-spin" />} text="Checking" />}
          {item.status === "checking_immich" && (
            <Badge icon={<Loader2 size={11} className="animate-spin" />} text="Checking Immich" />
          )}
          {item.status === "in_immich" && (
            <Badge
              icon={immichMatch?.verified ? <Check size={11} strokeWidth={3} /> : <Cloud size={11} />}
              text="In Immich"
            />
          )}
        </div>

        {outcome && (
          <div className="absolute bottom-2 right-2">
            {outcome.outcome === "failed" ? (
              <span className="flex h-6 w-6 items-center justify-center rounded-full bg-danger text-black">
                <CircleAlert size={15} />
              </span>
            ) : outcome.outcome !== "skipped" ? (
              <span className="flex h-6 w-6 items-center justify-center rounded-full bg-success text-black">
                <Check size={15} strokeWidth={3} />
              </span>
            ) : null}
          </div>
        )}

        <span className="absolute bottom-2 left-2 rounded bg-black/60 px-1.5 py-0.5 text-[10px] font-medium tracking-wide text-text-primary">
          {item.kind === "video" ? (
            <span className="flex items-center gap-1">
              <Play size={10} fill="currentColor" />
              {[exts, item.durationSecs != null ? formatClipLength(item.durationSecs) : null, resolutionLabel(item.width, item.height)]
                .filter(Boolean)
                .join(" · ")}
            </span>
          ) : (
            exts
          )}
        </span>
      </div>
      <div className="mt-1.5 flex items-center gap-1 px-0.5 text-[11px] leading-tight">
        <span className={`truncate ${dimmed ? "text-text-secondary" : ""}`}>{item.name}</span>
      </div>
      <div className="flex items-center gap-1 px-0.5 text-[10px] text-text-secondary">
        {item.dateSource && item.dateSource !== "exif" && (
          <span title={`No EXIF date; using file ${item.dateSource} time`}>
            <AlertTriangle size={10} className="text-warning" />
          </span>
        )}
        {item.capture ? (
          <span className="truncate">{formatCapture(item.capture)}</span>
        ) : (
          <span className="flex items-center gap-1">
            <Clock size={10} /> …
          </span>
        )}
      </div>
    </div>
  );
}

function Badge({ icon, text }: { icon: React.ReactNode; text: string }) {
  return (
    <span className="flex items-center gap-1 rounded bg-black/70 px-1.5 py-0.5 text-[10px] font-medium text-text-primary">
      {icon}
      {text}
    </span>
  );
}

export default memo(PhotoTile);
