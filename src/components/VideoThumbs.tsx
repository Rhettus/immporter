import { useEffect, useRef } from "react";
import { api, ItemView, mediaUrl } from "../api";
import { useStore } from "../store";

const CONCURRENCY = 2;
const TIMEOUT_MS = 20_000;
const MAX_EDGE = 640;

/**
 * Decodes one frame of each video with the web view's own decoder and hands
 * it to the backend as the tile's thumbnail. The video is streamed from the
 * card via `media://` (read-only, range requests), so only its first few MB
 * are read. Renders nothing.
 */
export default function VideoThumbs() {
  const gen = useStore((s) => s.gen);
  const items = useStore((s) => s.items);
  const busy = useRef(new Set<number>());
  const tried = useRef(new Set<number>());
  const genRef = useRef(gen);

  useEffect(() => {
    genRef.current = gen;
    busy.current.clear();
    tried.current.clear();
  }, [gen]);

  useEffect(() => {
    const todo = items.filter(
      (i) => i.kind === "video" && !i.thumb && !i.thumbFailed && !tried.current.has(i.id) && !busy.current.has(i.id),
    );
    for (const item of todo) {
      if (busy.current.size >= CONCURRENCY) break;
      busy.current.add(item.id);
      tried.current.add(item.id);
      const g = gen;
      grabFrame(g, item)
        .then((jpeg) => (genRef.current === g ? api.saveVideoThumb(g, item.id, jpeg) : undefined))
        .catch(() => {
          if (genRef.current === g) useStore.getState().markThumbFailed(item.id);
        })
        .finally(() => {
          busy.current.delete(item.id);
          // Kick the next one even if no item update arrives.
          useStore.setState((s) => ({ items: s.items.slice() }));
        });
    }
  }, [items, gen]);

  return null;
}

/** Points to try, as fractions of the clip; later ones if a frame is black. */
const SEEK_POINTS = [0.1, 0.3, 0.5];

function grabFrame(gen: number, item: ItemView): Promise<string> {
  return new Promise((resolve, reject) => {
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.preload = "auto";
    let done = false;
    let attempt = 0;
    const finish = (err: unknown, data?: string) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      video.removeAttribute("src");
      video.load();
      if (data) resolve(data);
      else reject(err);
    };
    const timer = setTimeout(() => finish(new Error("timeout")), TIMEOUT_MS);
    const seekTo = (i: number) => {
      const d = isFinite(video.duration) ? video.duration : 0;
      video.currentTime = Math.min(Math.max(d - 0.1, 0), d * SEEK_POINTS[i]);
    };
    // "seeked" can fire before the frame is painted; wait for a real frame.
    const whenFramePainted = (cb: () => void) => {
      const v = video as HTMLVideoElement & { requestVideoFrameCallback?: (cb: () => void) => number };
      if (v.requestVideoFrameCallback) {
        v.requestVideoFrameCallback(() => cb());
        // The callback only fires while frames are presented: nudge playback.
        video.play().then(() => video.pause()).catch(() => {});
      } else {
        requestAnimationFrame(() => requestAnimationFrame(cb));
      }
    };
    video.onerror = () => finish(video.error ?? new Error("decode failed"));
    video.onloadedmetadata = () => seekTo(0);
    video.onseeked = () =>
      whenFramePainted(() => {
        if (done) return;
        const w = video.videoWidth;
        const h = video.videoHeight;
        if (!w || !h) return finish(new Error("no frame"));
        const scale = Math.min(1, MAX_EDGE / Math.max(w, h));
        const canvas = document.createElement("canvas");
        canvas.width = Math.round(w * scale);
        canvas.height = Math.round(h * scale);
        const ctx = canvas.getContext("2d", { willReadFrequently: true });
        if (!ctx) return finish(new Error("no canvas"));
        try {
          ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
          if (isNearlyBlack(ctx, canvas.width, canvas.height) && attempt < SEEK_POINTS.length - 1) {
            attempt += 1;
            return seekTo(attempt);
          }
          finish(null, canvas.toDataURL("image/jpeg", 0.85));
        } catch (e) {
          finish(e);
        }
      });
    video.src = mediaUrl(gen, item.id);
  });
}

/** Average brightness below ~3%: a blank or not-yet-decoded frame. */
function isNearlyBlack(ctx: CanvasRenderingContext2D, w: number, h: number): boolean {
  const { data } = ctx.getImageData(0, 0, w, h);
  let sum = 0;
  let n = 0;
  for (let i = 0; i < data.length; i += 4 * 16) {
    sum += data[i] + data[i + 1] + data[i + 2];
    n += 3;
  }
  return n > 0 && sum / n < 8;
}
